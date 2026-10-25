//! The left dock: every changed file of the diff, as a tree or a flat list
//! (spec §9).
//!
//! - Rows are rendered with `uniform_list`, so only the visible ones are laid
//!   out; each row is built from labels pre-rendered in [`TreeIndex`].
//! - The file list and the tree index are computed on the tokio runtime; the
//!   +/- counts stream in from the diff session's worker pool. Counts never
//!   change the row order or height, so the list does not jump while they
//!   fill in ("…" until known).
//! - The collapsed directories are keyed by path and outlive the file list,
//!   so a refresh (e.g. after an Accept) keeps them; the selection is
//!   carried over by path too.
//! - The filter box fuzzy-matches paths (old paths of renames too). It is
//!   shown for lists longer than [`FILTER_MIN_FILES`], while it is in use,
//!   and when asked for (`/`); a short list needs no search.
//! - Files matching the hidden-file patterns ([`HiddenFiles`]) are left out
//!   entirely; their mask is computed on the tokio runtime with the list,
//!   and again when the patterns change.
//!
//! The diff view (#9) listens to [`FileTreeEvent::OpenFile`] and reports
//! its scroll position back through [`FileTree::set_active_file`]; the
//! comment UI (#14, #15) fills the per-file state markers and the
//! "Outdated" group through [`FileTree::set_file_markers`] and
//! [`FileTree::set_outdated`].
//!
//! Rows follow Panoptes #56 §5: a letter badge, the name, an optional state
//! marker (teal dot for a proposal, the Claude activity eye for a running
//! run) and the +/- counts in mono.

pub mod model;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use argus_diff::{ChangeKind, DiffOptions, DiffSession, FileList, FileStats, StatsEvent};
use gpui::{
    AnyElement, App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, Hsla, KeyDownEvent,
    Pixels, ScrollStrategy, SharedString, Task, UniformListScrollHandle, Window, div, prelude::*,
    px, uniform_list,
};

use crate::BackgroundJobs;
use crate::components::{BadgeSize, claude_activity, letter_badge, radius};
use crate::fuzzy::FuzzyQuery;
use crate::hidden_files::HiddenFiles;
use crate::icons::{IconName, icon};
use crate::keymap::{
    CollapseEntry, Dismiss, ExpandEntry, FILE_TREE_CONTEXT, OpenEntry, SelectNextEntry,
    SelectPreviousEntry, TEXT_INPUT_CONTEXT,
};
use crate::perf;
use crate::runtime::spawn_job;
use crate::session::DiffTarget;
use crate::theme::{ActiveTheme, ThemeColors};

use model::{Row, TreeIndex, View};

/// Height of every row (`uniform_list` needs one height).
const ROW_HEIGHT: f32 = 26.0;
/// Indentation per tree level.
const INDENT: f32 = 12.0;
/// Left padding of depth-0 rows.
const ROW_PADDING: f32 = 6.0;
/// Size of the directory carets.
const CARET_SIZE: Pixels = px(12.0);
/// Size of the proposal dot.
const PROPOSAL_DOT_SIZE: Pixels = px(6.0);
/// Size of the Claude activity eye in a row.
const ACTIVITY_SIZE: Pixels = px(13.0);
/// Size of the mono +/- counts.
const COUNTS_TEXT_SIZE: Pixels = px(11.0);
/// Size of row labels.
const ROW_TEXT_SIZE: Pixels = px(12.5);
/// Lists with more files than this show the filter box without asking.
const FILTER_MIN_FILES: usize = 15;

/// The state marker of a file row (Panoptes #56 §5). A file shows at most
/// one; [`FileMarker::Running`] wins over [`FileMarker::Proposed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileMarker {
    /// One of the file's comments has a proposal waiting for review.
    Proposed,
    /// A run is active on one of the file's comments.
    Running,
}

/// What the file tree reports to the workspace.
#[derive(Clone, Debug)]
pub enum FileTreeEvent {
    /// Show this file in the diff view.
    OpenFile {
        /// File-list index.
        index: usize,
        /// Path in head (merge-base path for deletions).
        path: SharedString,
    },
    /// Show this outdated comment (position in [`FileTree::set_outdated`]'s
    /// list).
    OpenOutdated(usize),
    /// The user switched between tree and flat list; worth persisting.
    ModeChanged {
        /// Flat list rather than tree.
        flat: bool,
    },
}

/// Loading state of the file list.
#[derive(Clone, Debug)]
enum Status {
    /// No diff target.
    Idle,
    /// The list for the current target is being computed. A previous list,
    /// if any, stays visible meanwhile.
    Loading,
    /// The list is current.
    Ready,
    /// Computing the list failed.
    Failed(SharedString),
}

/// The selected row's subject, kept across rebuilds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selection {
    /// A file, by file-list index.
    File(usize),
    /// A directory, by index in the tree index.
    Dir(usize),
    /// An outdated comment.
    Outdated(usize),
}

/// A computed file list with its tree index.
struct Loaded {
    list: Arc<FileList>,
    index: Arc<TreeIndex>,
    /// By file-list index: matched by a hidden-file pattern.
    hidden: Arc<Vec<bool>>,
}

/// The file tree dock.
pub struct FileTree {
    focus_handle: FocusHandle,
    filter_focus: FocusHandle,
    status: Status,
    session: Option<DiffSession>,
    list: Option<Arc<FileList>>,
    index: Arc<TreeIndex>,
    hidden_files: Arc<HiddenFiles>,
    /// By file-list index: left out of the rows.
    hidden: Arc<Vec<bool>>,
    /// How many entries of `hidden` are set.
    hidden_count: usize,
    hidden_task: Option<Task<()>>,
    stats: Vec<Option<FileStats>>,
    markers: HashMap<SharedString, FileMarker>,
    outdated: Vec<SharedString>,
    flat: bool,
    query: String,
    fuzzy: FuzzyQuery,
    /// The filter box was asked for (`/`) on a short list.
    filter_requested: bool,
    collapsed: HashSet<SharedString>,
    rows: Vec<Row>,
    selection: Option<Selection>,
    selected_row: Option<usize>,
    scroll: UniformListScrollHandle,
    loading_flag: bool,
    load_task: Option<Task<()>>,
    stats_task: Option<Task<()>>,
}

impl EventEmitter<FileTreeEvent> for FileTree {}

impl Focusable for FileTree {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl FileTree {
    /// An empty tree; `flat` restores the persisted list mode and
    /// `hidden_files` leaves the matching files out.
    pub fn new(flat: bool, hidden_files: Arc<HiddenFiles>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            filter_focus: cx.focus_handle(),
            status: Status::Idle,
            session: None,
            list: None,
            index: Arc::new(TreeIndex::default()),
            hidden_files,
            hidden: Arc::default(),
            hidden_count: 0,
            hidden_task: None,
            stats: Vec::new(),
            markers: HashMap::new(),
            outdated: Vec::new(),
            flat,
            query: String::new(),
            fuzzy: FuzzyQuery::default(),
            filter_requested: false,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selection: None,
            selected_row: None,
            scroll: UniformListScrollHandle::new(),
            loading_flag: false,
            load_task: None,
            stats_task: None,
        }
    }

    // ---- accessors ----------------------------------------------------------

    /// The diff session of the current target; the diff view (#9) shares
    /// it, so hunks come from the same commits and cache.
    pub fn diff_session(&self) -> Option<&DiffSession> {
        self.session.as_ref()
    }

    /// The current file list, once computed.
    pub fn file_list(&self) -> Option<&Arc<FileList>> {
        self.list.as_ref()
    }

    /// The selected file's index, if a file row is selected.
    pub fn selected_file(&self) -> Option<usize> {
        match self.selection {
            Some(Selection::File(index)) => Some(index),
            _ => None,
        }
    }

    /// The filter box's focus handle.
    pub fn filter_focus_handle(&self) -> &FocusHandle {
        &self.filter_focus
    }

    /// Shows the filter box even on a short list, before it gets the focus.
    pub fn request_filter(&mut self, cx: &mut Context<Self>) {
        if !self.filter_requested {
            self.filter_requested = true;
            cx.notify();
        }
    }

    /// Whether the filter box is shown.
    fn shows_filter(&self, window: &Window) -> bool {
        let files = self.index.len().saturating_sub(self.hidden_count());
        files > FILTER_MIN_FILES
            || self.filter_requested
            || !self.query.is_empty()
            || self.filter_focus.is_focused(window)
    }

    /// Whether the flat list is shown instead of the tree.
    pub fn is_flat(&self) -> bool {
        self.flat
    }

    /// Number of changed files left out by the hidden-file patterns.
    pub fn hidden_count(&self) -> usize {
        self.hidden_count
    }

    fn set_hidden(&mut self, hidden: Arc<Vec<bool>>) {
        self.hidden_count = hidden.iter().filter(|&&hidden| hidden).count();
        self.hidden = hidden;
    }

    // ---- loading ------------------------------------------------------------

    /// Follows the session's diff target: a new target starts a new diff
    /// session (the previous one, and its jobs, are cancelled); `None`
    /// stops loading but keeps the last list, so a refresh does not blank
    /// the dock.
    pub fn set_target(
        &mut self,
        repo: &argus_git::Repo,
        target: Option<&DiffTarget>,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = target else {
            self.session = None;
            self.load_task = None;
            self.stats_task = None;
            self.set_loading_flag(false, cx);
            self.status = Status::Idle;
            cx.notify();
            return;
        };
        if let Some(session) = &self.session
            && session.merge_base() == target.merge_base
            && session.head() == target.head_tip
        {
            return;
        }
        let session = DiffSession::between(
            repo,
            target.merge_base,
            target.head_tip,
            DiffOptions::default(),
        );
        self.session = Some(session.clone());
        self.stats_task = None;
        self.status = Status::Loading;
        self.set_loading_flag(true, cx);
        let hidden_files = self.hidden_files.clone();
        let job = spawn_job(cx, async move {
            let list = session.file_list().await?;
            // Building the index walks every path; keep it off the UI thread.
            let index = Arc::new(TreeIndex::new(list.entries()));
            let hidden = Arc::new(hidden_files.mask(&list));
            Ok::<_, argus_diff::DiffError>(Loaded {
                list,
                index,
                hidden,
            })
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.load_task = None;
                this.set_loading_flag(false, cx);
                match result {
                    Ok(Ok(loaded)) => this.install(loaded, cx),
                    Ok(Err(argus_diff::DiffError::Cancelled)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "failed to list changed files");
                        this.status = Status::Failed(error.to_string().into());
                    }
                    Err(error) => {
                        if !error.is_cancelled() {
                            this.status = Status::Failed(error.to_string().into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Cancels every job, e.g. before the repository is replaced.
    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        self.session = None;
        self.load_task = None;
        self.stats_task = None;
        self.set_loading_flag(false, cx);
    }

    fn set_loading_flag(&mut self, loading: bool, cx: &mut Context<Self>) {
        if self.loading_flag != loading {
            self.loading_flag = loading;
            BackgroundJobs::update(cx, |jobs| jobs.listing_files = loading);
        }
    }

    /// Replaces the list, carrying the selection over by path, and starts
    /// following the counts.
    fn install(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        let selected_path = self.selection_path();
        let Loaded {
            list,
            index,
            hidden,
        } = loaded;
        tracing::debug!(files = list.len(), "file tree loaded");
        perf::file_list_computed(list.len(), cx);
        self.stats = vec![None; list.len()];
        self.index = index;
        self.set_hidden(hidden);
        self.hidden_task = None;
        self.list = Some(list.clone());
        self.status = Status::Ready;
        self.selection = selected_path.and_then(|(is_dir, path)| {
            if is_dir {
                self.index.dir_by_path(&path).map(Selection::Dir)
            } else {
                list.index_of(&path).map(Selection::File)
            }
        });
        self.rebuild_rows();
        if let Some(row) = self.selected_row {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }

        if let Some(session) = &self.session {
            let mut updates = session.stats_updates();
            self.stats_task = Some(cx.spawn(async move |this, cx| {
                while let Some(first) = updates.recv().await {
                    // Coalesce whatever else is queued into one repaint.
                    let mut events = vec![first];
                    while let Ok(event) = updates.try_recv() {
                        events.push(event);
                    }
                    let updated = this.update(cx, |this, cx| {
                        for event in events {
                            if let StatsEvent::Batch(batch) = event {
                                for (index, stats) in batch {
                                    if let Some(slot) = this.stats.get_mut(index) {
                                        *slot = Some(stats);
                                    }
                                }
                            }
                        }
                        cx.notify();
                    });
                    if updated.is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// `(is_dir, path)` of the selection.
    fn selection_path(&self) -> Option<(bool, String)> {
        match self.selection? {
            Selection::File(index) => self
                .index
                .file(index)
                .map(|file| (false, file.path.to_string())),
            Selection::Dir(dir) => self.index.dir(dir).map(|dir| (true, dir.path.to_string())),
            Selection::Outdated(_) => None,
        }
    }

    // ---- inputs from other views ------------------------------------------

    /// Applies new hidden-file patterns: the mask of the current list is
    /// recomputed on the tokio runtime, then the rows are rebuilt.
    pub fn set_hidden_files(&mut self, hidden_files: Arc<HiddenFiles>, cx: &mut Context<Self>) {
        self.hidden_files = hidden_files.clone();
        let Some(list) = self.list.clone() else {
            return;
        };
        let job = spawn_job(cx, async move { hidden_files.mask(&list) });
        self.hidden_task = Some(cx.spawn(async move |this, cx| {
            let Ok(mask) = job.await else {
                return;
            };
            // A list installed meanwhile drops this task with its own mask.
            this.update(cx, |this, cx| {
                this.hidden_task = None;
                this.set_hidden(Arc::new(mask));
                this.after_filter_change(cx);
            })
            .ok();
        }));
    }

    /// State markers by file path (#14): files without an entry show none.
    pub fn set_file_markers(
        &mut self,
        markers: HashMap<SharedString, FileMarker>,
        cx: &mut Context<Self>,
    ) {
        if self.markers != markers {
            self.markers = markers;
            cx.notify();
        }
    }

    /// The outdated comments listed in the bottom group (#15); the group is
    /// hidden when empty.
    pub fn set_outdated(&mut self, items: Vec<SharedString>, cx: &mut Context<Self>) {
        self.outdated = items;
        if matches!(self.selection, Some(Selection::Outdated(i)) if i >= self.outdated.len()) {
            self.selection = None;
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Highlights the file the diff view is scrolled to, without opening it
    /// or expanding collapsed directories.
    pub fn set_active_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selection == Some(Selection::File(index)) {
            return;
        }
        self.selection = Some(Selection::File(index));
        self.selected_row = self.row_of(Selection::File(index));
        if let Some(row) = self.selected_row {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    // ---- rows ---------------------------------------------------------------

    fn view(&self) -> View<'_> {
        View {
            flat: self.flat,
            query: &self.fuzzy,
            hidden: &self.hidden,
            collapsed: &self.collapsed,
            outdated: self.outdated.len(),
        }
    }

    fn filtering(&self) -> bool {
        !self.fuzzy.is_empty()
    }

    /// Recomputes the visible rows and the selected row.
    fn rebuild_rows(&mut self) {
        let started = Instant::now();
        self.rows = self.index.rows(&self.view());
        self.selected_row = self.selection.and_then(|selection| self.row_of(selection));
        tracing::trace!(
            rows = self.rows.len(),
            elapsed_us = started.elapsed().as_micros() as u64,
            "rebuilt file tree rows"
        );
    }

    fn row_of(&self, selection: Selection) -> Option<usize> {
        self.rows.iter().position(|row| match (selection, *row) {
            (Selection::File(a), Row::File { index, .. }) => a == index,
            (Selection::Dir(a), Row::Dir { dir, .. }) => a == dir,
            (Selection::Outdated(a), Row::Outdated(b)) => a == b,
            _ => false,
        })
    }

    fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let selection = match self.rows.get(row) {
            Some(Row::File { index, .. }) => Selection::File(*index),
            Some(Row::Dir { dir, .. }) => Selection::Dir(*dir),
            Some(Row::Outdated(item)) => Selection::Outdated(*item),
            Some(Row::OutdatedHeader { .. }) | None => return,
        };
        self.selection = Some(selection);
        self.selected_row = Some(row);
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Moves the selection by `delta` rows, skipping group headers.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let mut row = match self.selected_row {
            Some(row) => row as isize + delta,
            None if delta > 0 => 0,
            None => self.rows.len() as isize - 1,
        };
        while let Some(Row::OutdatedHeader { .. }) =
            usize::try_from(row).ok().and_then(|row| self.rows.get(row))
        {
            row += delta.signum();
        }
        let row = row.clamp(0, self.rows.len() as isize - 1) as usize;
        if matches!(self.rows[row], Row::OutdatedHeader { .. }) {
            return;
        }
        self.select_row(row, cx);
    }

    fn set_collapsed(&mut self, dir: usize, collapsed: bool, cx: &mut Context<Self>) {
        let Some(path) = self.index.dir(dir).map(|dir| dir.path.clone()) else {
            return;
        };
        let changed = if collapsed {
            self.collapsed.insert(path)
        } else {
            self.collapsed.remove(&path)
        };
        if changed {
            self.rebuild_rows();
            cx.notify();
        }
    }

    /// Opens the row: files go to the diff view, directories toggle.
    fn activate_row(&mut self, row: usize, cx: &mut Context<Self>) {
        match self.rows.get(row).copied() {
            Some(Row::File { index, .. }) => {
                self.select_row(row, cx);
                self.emit_open(index, cx);
            }
            Some(Row::Dir { dir, expanded, .. }) => {
                self.select_row(row, cx);
                if !self.filtering() {
                    self.set_collapsed(dir, expanded, cx);
                }
            }
            Some(Row::Outdated(item)) => {
                self.select_row(row, cx);
                cx.emit(FileTreeEvent::OpenOutdated(item));
            }
            Some(Row::OutdatedHeader { .. }) | None => {}
        }
    }

    fn emit_open(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(file) = self.index.file(index) {
            cx.emit(FileTreeEvent::OpenFile {
                index,
                path: file.path.clone(),
            });
        }
    }

    /// Selects the next (`delta > 0`) or previous visible file in display
    /// order, expanding its directories, and opens it (`j` / `k`).
    pub fn select_adjacent_file(&mut self, delta: isize, cx: &mut Context<Self>) {
        let order = self.index.file_order(&self.view());
        if order.is_empty() {
            return;
        }
        let current = self
            .selected_file()
            .and_then(|file| order.iter().position(|candidate| *candidate == file));
        let position = match current {
            Some(position) => (position as isize + delta).clamp(0, order.len() as isize - 1),
            None if delta > 0 => 0,
            None => order.len() as isize - 1,
        } as usize;
        let index = order[position];
        if current == Some(position) {
            return;
        }
        self.reveal(index);
        self.selection = Some(Selection::File(index));
        self.rebuild_rows();
        if let Some(row) = self.selected_row {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        self.emit_open(index, cx);
        cx.notify();
    }

    /// Expands every directory above file `index`.
    fn reveal(&mut self, index: usize) {
        for dir in self.index.ancestors_of_file(index) {
            if let Some(node) = self.index.dir(dir) {
                self.collapsed.remove(&node.path);
            }
        }
    }

    /// Switches between tree and flat list.
    pub fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.set_flat(!self.flat, cx);
    }

    fn set_flat(&mut self, flat: bool, cx: &mut Context<Self>) {
        if self.flat == flat {
            return;
        }
        self.flat = flat;
        if flat && matches!(self.selection, Some(Selection::Dir(_))) {
            self.selection = None;
        }
        if let Some(Selection::File(index)) = self.selection
            && !flat
        {
            self.reveal(index);
        }
        self.rebuild_rows();
        if let Some(row) = self.selected_row {
            self.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
        cx.emit(FileTreeEvent::ModeChanged { flat });
        cx.notify();
    }

    // ---- filter -------------------------------------------------------------

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if query == self.query {
            return;
        }
        self.query = query;
        self.fuzzy = FuzzyQuery::new(&self.query);
        self.after_filter_change(cx);
    }

    /// Rebuilds the rows; when the selection got filtered out, the best
    /// match is selected so Enter opens it.
    fn after_filter_change(&mut self, cx: &mut Context<Self>) {
        self.rebuild_rows();
        if self.selected_row.is_none() {
            let first_file = self
                .rows
                .iter()
                .position(|row| matches!(row, Row::File { .. }));
            if let Some(row) = first_file {
                self.select_row(row, cx);
            }
        }
        if let Some(row) = self.selected_row {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn on_filter_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        match keystroke.key.as_str() {
            "up" => self.move_selection(-1, cx),
            "down" => self.move_selection(1, cx),
            "pageup" => self.move_selection(-10, cx),
            "pagedown" => self.move_selection(10, cx),
            "enter" => {
                if let Some(row) = self.selected_row {
                    self.activate_row(row, cx);
                }
                window.focus(&self.focus_handle, cx);
            }
            "backspace" if modifiers.secondary() || modifiers.alt => {
                self.set_query(String::new(), cx);
            }
            "backspace" => {
                let mut query = self.query.clone();
                query.pop();
                self.set_query(query, cx);
            }
            "v" if modifiers.secondary() => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let line = text.lines().next().unwrap_or_default().trim();
                    let query = format!("{}{line}", self.query);
                    self.set_query(query, cx);
                }
            }
            _ => {
                if modifiers.control || modifiers.platform || modifiers.function {
                    return;
                }
                let Some(text) = keystroke.key_char.as_deref() else {
                    return;
                };
                if text.chars().any(char::is_control) {
                    return;
                }
                let query = format!("{}{text}", self.query);
                self.set_query(query, cx);
            }
        }
        cx.stop_propagation();
    }

    /// Escape in the filter: clear it, then leave it (and hide it again on
    /// a short list).
    fn filter_dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.query.is_empty() {
            self.filter_requested = false;
            window.focus(&self.focus_handle, cx);
            cx.notify();
        } else {
            self.set_query(String::new(), cx);
        }
    }

    // ---- actions ------------------------------------------------------------

    fn select_next(&mut self, _: &SelectNextEntry, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_previous(&mut self, _: &SelectPreviousEntry, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn open_entry(&mut self, _: &OpenEntry, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row {
            self.activate_row(row, cx);
        }
    }

    /// Left: collapse an expanded directory, otherwise go to the parent.
    fn collapse_entry(&mut self, _: &CollapseEntry, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row else {
            return;
        };
        let parent = match self.rows[row] {
            Row::Dir { dir, expanded, .. } => {
                if expanded && !self.filtering() {
                    self.set_collapsed(dir, true, cx);
                    return;
                }
                self.index.dir(dir).and_then(|node| node.parent)
            }
            Row::File { index, .. } if !self.flat => self.index.file(index).map(|file| file.parent),
            _ => None,
        };
        if let Some(parent) = parent.filter(|parent| *parent != self.index.root())
            && let Some(parent_row) = self.row_of(Selection::Dir(parent))
        {
            self.select_row(parent_row, cx);
        }
    }

    /// Right: expand a collapsed directory, otherwise go to its first child.
    fn expand_entry(&mut self, _: &ExpandEntry, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row else {
            return;
        };
        if let Row::Dir {
            dir,
            expanded,
            depth,
        } = self.rows[row]
        {
            if !expanded {
                self.set_collapsed(dir, false, cx);
            } else if let Some(Row::Dir { depth: child, .. } | Row::File { depth: child, .. }) =
                self.rows.get(row + 1)
                && *child > depth
            {
                self.select_row(row + 1, cx);
            }
        }
    }

    // ---- rendering ----------------------------------------------------------

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if !range.is_empty() {
            perf::mark_painted(perf::Milestone::FileTreePainted, cx);
        }
        let colors = cx.theme().colors.clone();
        let code_family = cx.theme().fonts.code_family.clone();
        let mut elements = Vec::with_capacity(range.len());
        for row_index in range {
            let Some(row) = self.rows.get(row_index).copied() else {
                break;
            };
            let selected = self.selected_row == Some(row_index);
            let base = div()
                .id(("file-tree-row", row_index))
                .flex()
                .w_full()
                .items_center()
                .gap_2()
                .h(px(ROW_HEIGHT))
                .pr(px(6.0))
                .rounded(radius::sm(cx))
                .text_size(ROW_TEXT_SIZE)
                .whitespace_nowrap()
                .cursor_pointer()
                .when(selected, |row| row.bg(colors.element_selected))
                .when(!selected, |row| {
                    row.hover(|style| style.bg(colors.element_hover))
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    this.activate_row(row_index, cx);
                }));
            let element = match row {
                Row::Dir {
                    dir,
                    depth,
                    expanded,
                } => {
                    let label = self
                        .index
                        .dir(dir)
                        .map(|dir| dir.label.clone())
                        .unwrap_or_default();
                    let caret = if expanded {
                        IconName::CaretDown
                    } else {
                        IconName::CaretRight
                    };
                    base.pl(px(ROW_PADDING + depth as f32 * INDENT))
                        .gap(px(6.0))
                        .child(icon(caret, CARET_SIZE, colors.text_muted))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(colors.text_muted)
                                .child(label),
                        )
                }
                Row::File { index, depth } => {
                    self.render_file_row(base, index, depth, selected, cx)
                }
                Row::OutdatedHeader { count } => div()
                    .id(("file-tree-row", row_index))
                    .flex()
                    .w_full()
                    .items_center()
                    .gap(px(6.0))
                    .h(px(ROW_HEIGHT))
                    .px(px(ROW_PADDING))
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .text_size(ROW_TEXT_SIZE)
                    .text_color(colors.text_muted)
                    .child(icon(IconName::Warning, CARET_SIZE, colors.warning))
                    .child(div().flex_1().child("Outdated"))
                    .child(
                        div()
                            .font_family(code_family.clone())
                            .text_size(COUNTS_TEXT_SIZE)
                            .text_color(colors.text_placeholder)
                            .child(count.to_string()),
                    ),
                Row::Outdated(item) => base
                    .pl(px(ROW_PADDING + INDENT))
                    .text_color(colors.text_muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(self.outdated.get(item).cloned().unwrap_or_default()),
                    ),
            };
            elements.push(element.into_any_element());
        }
        elements
    }

    fn render_file_row(
        &self,
        base: gpui::Stateful<gpui::Div>,
        index: usize,
        depth: usize,
        selected: bool,
        cx: &App,
    ) -> gpui::Stateful<gpui::Div> {
        let Some(file) = self.index.file(index) else {
            return base;
        };
        let colors = &cx.theme().colors;
        let label = if self.flat {
            file.flat_label.clone()
        } else {
            file.tree_label.clone()
        };
        let name_color = if selected {
            colors.text
        } else if file.kind == ChangeKind::Deleted {
            colors.text_muted
        } else {
            body_text(colors)
        };
        // Files sit one caret-width deeper than their directory's label.
        let indent = if self.flat {
            ROW_PADDING
        } else {
            ROW_PADDING + depth as f32 * INDENT + 4.0
        };
        let marker: Option<AnyElement> = match self.markers.get(&file.path) {
            Some(FileMarker::Running) => Some(claude_activity(ACTIVITY_SIZE, cx)),
            Some(FileMarker::Proposed) => Some(
                div()
                    .flex_none()
                    .size(PROPOSAL_DOT_SIZE)
                    .rounded_full()
                    .bg(colors.accent)
                    .into_any_element(),
            ),
            None => None,
        };
        base.pl(px(indent))
            .child(letter_badge(file.kind, BadgeSize::Small, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(name_color)
                    .child(label),
            )
            .children(marker)
            .child(render_counts(
                self.stats.get(index).and_then(Option::as_ref),
                cx,
            ))
    }

    fn render_filter(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let focused = self.filter_focus.is_focused(window);
        let accent = colors.accent;
        // A plain caret, since there is no text-input element yet.
        let caret = move || div().flex_none().w(px(1.5)).h(px(14.0)).bg(accent);
        let text: SharedString = if self.query.is_empty() {
            "Filter files…".into()
        } else {
            self.query.clone().into()
        };
        let input = div()
            .id("file-tree-filter")
            .key_context(TEXT_INPUT_CONTEXT)
            .track_focus(&self.filter_focus)
            .on_key_down(cx.listener(Self::on_filter_key_down))
            .on_action(cx.listener(Self::filter_dismiss))
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap_0p5()
            .h(px(24.0))
            .px_2()
            .rounded(radius::sm(cx))
            .border_1()
            .border_color(if focused {
                colors.accent
            } else {
                colors.border
            })
            .bg(colors.background)
            .cursor_text()
            .text_sm()
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.filter_focus, cx);
            }))
            .when(focused && self.query.is_empty(), |row| row.child(caret()))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(if self.query.is_empty() {
                        colors.text_placeholder
                    } else {
                        colors.text
                    })
                    .child(text),
            )
            .when(focused && !self.query.is_empty(), |row| row.child(caret()));

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .child(input)
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let message = |text: SharedString, color: Hsla| {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .justify_center()
                .p_4()
                .text_sm()
                .text_center()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        if let Status::Failed(error) = &self.status {
            return message(
                format!("Could not list the changed files: {error}").into(),
                colors.error,
            );
        }
        if self.list.is_none() {
            let text = match self.status {
                Status::Loading => "Computing the changed files…",
                _ => "No diff loaded.",
            };
            return message(text.into(), colors.text_placeholder);
        }
        if self.rows.is_empty() {
            let text = if self.index.is_empty() {
                "No changes between base and head."
            } else if self.hidden_count == self.index.len() {
                "Every file is hidden."
            } else {
                "No file matches the filter."
            };
            return message(text.into(), colors.text_placeholder);
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                uniform_list(
                    "file-tree-rows",
                    self.rows.len(),
                    cx.processor(|this, range, _, cx| this.render_rows(range, cx)),
                )
                .track_scroll(&self.scroll)
                .size_full()
                .px_2()
                .py_1(),
            )
            .into_any_element()
    }
}

/// The name color of an unselected row: `text_muted` brightened to the body
/// color (Panoptes #56 §5), i.e. primary text slightly sunk into the dock.
fn body_text(colors: &ThemeColors) -> Hsla {
    colors.text_muted.blend(colors.text.opacity(0.75))
}

/// `+n −m` in mono, or "…" while the counts are being computed. A zero side
/// is drawn in `text_placeholder`.
fn render_counts(stats: Option<&FileStats>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let colors = &theme.colors;
    let counts = div()
        .flex()
        .flex_none()
        .gap_1()
        .font_family(theme.fonts.code_family.clone())
        .text_size(COUNTS_TEXT_SIZE);
    let side = |count: u32, prefix: char, color: Hsla| {
        div()
            .text_color(if count == 0 {
                colors.text_placeholder
            } else {
                color
            })
            .child(format!("{prefix}{count}"))
    };
    match stats {
        None => counts.text_color(colors.text_placeholder).child("…"),
        Some(FileStats::Text { added, removed }) if *added == 0 && *removed == 0 => counts,
        Some(FileStats::Text { added, removed }) => counts
            .child(side(*added, '+', colors.diff_added_text))
            .child(side(*removed, '−', colors.diff_removed_text)),
        Some(FileStats::Binary { .. }) => counts.text_color(colors.text_muted).child("bin"),
        Some(FileStats::TooLarge { .. }) => counts.text_color(colors.text_muted).child("large"),
        Some(FileStats::Failed(_)) => counts.text_color(colors.error).child("!"),
    }
}

impl Render for FileTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let renames_limited = self
            .list
            .as_ref()
            .is_some_and(|list| list.renames_limited());
        let colors = cx.theme().colors.clone();
        div()
            .id("file-tree")
            .key_context(FILE_TREE_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::open_entry))
            .on_action(cx.listener(Self::collapse_entry))
            .on_action(cx.listener(Self::expand_entry))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .when(self.shows_filter(window), |tree| {
                tree.child(self.render_filter(window, cx))
            })
            .when(renames_limited, |tree| {
                tree.child(
                    div()
                        .px_3()
                        .pb_1()
                        .text_xs()
                        .text_color(colors.warning)
                        .child("Too many files for full rename detection."),
                )
            })
            .child(self.render_body(cx))
    }
}
