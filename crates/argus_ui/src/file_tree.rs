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
//! - The filter box fuzzy-matches paths (old paths of renames too); the
//!   A/M/D/R chips restrict change kinds.
//!
//! The diff view (#9) listens to [`FileTreeEvent::OpenFile`] and reports
//! its scroll position back through [`FileTree::set_active_file`]; the
//! comment UI (#14, #15) fills the badges and the "Outdated" group through
//! [`FileTree::set_comment_counts`] and [`FileTree::set_outdated`].

pub mod model;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use argus_diff::{ChangeKind, DiffOptions, DiffSession, FileList, FileStats, StatsEvent};
use gpui::{
    AnyElement, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, Hsla, KeyDownEvent,
    ScrollStrategy, SharedString, Task, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};

use crate::BackgroundJobs;
use crate::components::toggle_button;
use crate::fuzzy::FuzzyQuery;
use crate::keymap::{
    CollapseEntry, Dismiss, ExpandEntry, FILE_TREE_CONTEXT, OpenEntry, SelectNextEntry,
    SelectPreviousEntry, TEXT_INPUT_CONTEXT,
};
use crate::runtime::spawn_job;
use crate::session::DiffTarget;
use crate::theme::{ActiveTheme, ThemeColors};

use model::{KindFilter, KindGroup, Row, TreeIndex, View, kind_letter};

/// Height of every row (`uniform_list` needs one height).
const ROW_HEIGHT: f32 = 24.0;
/// Indentation per tree level.
const INDENT: f32 = 12.0;
/// Left padding of depth-0 rows.
const ROW_PADDING: f32 = 8.0;

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
}

/// The file tree dock.
pub struct FileTree {
    focus_handle: FocusHandle,
    filter_focus: FocusHandle,
    status: Status,
    session: Option<DiffSession>,
    list: Option<Arc<FileList>>,
    index: Arc<TreeIndex>,
    stats: Vec<Option<FileStats>>,
    comment_counts: HashMap<SharedString, u32>,
    outdated: Vec<SharedString>,
    flat: bool,
    query: String,
    fuzzy: FuzzyQuery,
    kinds: KindFilter,
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
    /// An empty tree; `flat` restores the persisted list mode.
    pub fn new(flat: bool, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            filter_focus: cx.focus_handle(),
            status: Status::Idle,
            session: None,
            list: None,
            index: Arc::new(TreeIndex::default()),
            stats: Vec::new(),
            comment_counts: HashMap::new(),
            outdated: Vec::new(),
            flat,
            query: String::new(),
            fuzzy: FuzzyQuery::default(),
            kinds: KindFilter::default(),
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

    /// Whether the flat list is shown instead of the tree.
    pub fn is_flat(&self) -> bool {
        self.flat
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
        let job = spawn_job(cx, async move {
            let list = session.file_list().await?;
            // Building the index walks every path; keep it off the UI thread.
            let index = Arc::new(TreeIndex::new(list.entries()));
            Ok::<_, argus_diff::DiffError>(Loaded { list, index })
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
        let Loaded { list, index } = loaded;
        tracing::debug!(files = list.len(), "file tree loaded");
        self.stats = vec![None; list.len()];
        self.index = index;
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

    /// Comment counts by file path, shown as badges (#14).
    pub fn set_comment_counts(
        &mut self,
        counts: HashMap<SharedString, u32>,
        cx: &mut Context<Self>,
    ) {
        self.comment_counts = counts;
        cx.notify();
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
            kinds: self.kinds,
            collapsed: &self.collapsed,
            outdated: self.outdated.len(),
        }
    }

    fn filtering(&self) -> bool {
        !self.fuzzy.is_empty() || !self.kinds.is_empty()
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

    fn toggle_kind(&mut self, group: KindGroup, cx: &mut Context<Self>) {
        self.kinds.toggle(group);
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

    /// Escape in the filter: clear it, then leave it.
    fn filter_dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        if self.query.is_empty() {
            window.focus(&self.focus_handle, cx);
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
        let colors = cx.theme().colors.clone();
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
                .gap_1()
                .h(px(ROW_HEIGHT))
                .pr_2()
                .text_sm()
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
                    base.pl(px(ROW_PADDING + depth as f32 * INDENT))
                        .child(
                            div()
                                .flex_none()
                                .w(px(12.0))
                                .text_xs()
                                .text_color(colors.text_muted)
                                .child(if expanded { "▾" } else { "▸" }),
                        )
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
                Row::File { index, depth } => self.render_file_row(base, index, depth, &colors),
                Row::OutdatedHeader { count } => div()
                    .id(("file-tree-row", row_index))
                    .flex()
                    .w_full()
                    .items_end()
                    .justify_between()
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .pb_0p5()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .text_xs()
                    .text_color(colors.warning)
                    .child("OUTDATED")
                    .child(count.to_string()),
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
        colors: &ThemeColors,
    ) -> gpui::Stateful<gpui::Div> {
        let Some(file) = self.index.file(index) else {
            return base;
        };
        let label = if self.flat {
            file.flat_label.clone()
        } else {
            file.tree_label.clone()
        };
        let kind_color = kind_color(file.kind, colors);
        let comments = self.comment_counts.get(&file.path).copied().unwrap_or(0);
        // Files sit one chevron-width deeper than their directory's label.
        let indent = if self.flat {
            ROW_PADDING
        } else {
            ROW_PADDING + depth as f32 * INDENT + 4.0
        };
        base.pl(px(indent))
            .child(
                div()
                    .flex_none()
                    .w(px(12.0))
                    .text_xs()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(kind_color)
                    .child(kind_letter(file.kind)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(if file.kind == ChangeKind::Deleted {
                        colors.text_muted
                    } else {
                        colors.text
                    })
                    .child(label),
            )
            .when(comments > 0, |row| {
                row.child(
                    div()
                        .flex_none()
                        .px_1()
                        .rounded_full()
                        .bg(colors.accent)
                        .text_xs()
                        .text_color(colors.accent_text)
                        .child(comments.to_string()),
                )
            })
            .child(render_counts(
                self.stats.get(index).and_then(Option::as_ref),
                colors,
            ))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let total = self.index.len();
        let summary: SharedString = match &self.status {
            Status::Loading if self.list.is_some() => "Refreshing…".into(),
            Status::Loading => "Loading…".into(),
            _ if self.list.is_none() => SharedString::default(),
            _ if self.filtering() => {
                let shown = self
                    .rows
                    .iter()
                    .filter(|row| matches!(row, Row::File { .. }))
                    .count();
                format!("{shown} of {total}").into()
            }
            _ => format!("{total} {}", if total == 1 { "file" } else { "files" }).into(),
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap_1()
            .h(px(32.0))
            .pl_3()
            .pr_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .text_xs()
            .text_color(colors.text_muted)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child("FILES")
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(colors.text_placeholder)
                            .child(summary),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .child(
                        toggle_button("file-tree-mode-tree", "Tree", !self.flat, cx)
                            .h(px(20.0))
                            .text_xs()
                            .on_click(cx.listener(|this, _, _, cx| this.set_flat(false, cx))),
                    )
                    .child(
                        toggle_button("file-tree-mode-flat", "List", self.flat, cx)
                            .h(px(20.0))
                            .text_xs()
                            .on_click(cx.listener(|this, _, _, cx| this.set_flat(true, cx))),
                    ),
            )
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
            .rounded_md()
            .border_1()
            .border_color(if focused {
                colors.accent
            } else {
                colors.border_variant
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

        let chips = KindGroup::ALL.into_iter().map(|group| {
            let active = self.kinds.contains(group);
            let color = kind_color(
                match group {
                    KindGroup::Added => ChangeKind::Added,
                    KindGroup::Modified => ChangeKind::Modified,
                    KindGroup::Deleted => ChangeKind::Deleted,
                    KindGroup::Renamed => ChangeKind::Renamed,
                },
                &colors,
            );
            div()
                .id(SharedString::from(format!("file-kind-{}", group.letter())))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(20.0))
                .rounded_md()
                .text_xs()
                .font_weight(gpui::FontWeight::BOLD)
                .cursor_pointer()
                .text_color(if active {
                    color
                } else {
                    colors.text_placeholder
                })
                .when(active, |chip| chip.bg(colors.element_selected))
                .hover(|style| style.bg(colors.element_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_kind(group, cx)))
                .child(group.letter())
        });

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .child(input)
            .children(chips)
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
                .py_1(),
            )
            .into_any_element()
    }
}

/// The colour of a change-kind letter.
fn kind_color(kind: ChangeKind, colors: &ThemeColors) -> Hsla {
    match kind {
        ChangeKind::Added => colors.diff_added_text,
        ChangeKind::Deleted => colors.diff_removed_text,
        ChangeKind::Renamed => colors.accent,
        ChangeKind::Modified | ChangeKind::ModeChanged | ChangeKind::TypeChanged => colors.warning,
    }
}

/// `+n −m`, or "…" while the counts are being computed.
fn render_counts(stats: Option<&FileStats>, colors: &ThemeColors) -> impl IntoElement {
    let counts = div().flex().flex_none().gap_1().text_xs();
    match stats {
        None => counts.text_color(colors.text_placeholder).child("…"),
        Some(FileStats::Text { added, removed }) if *added == 0 && *removed == 0 => counts,
        Some(FileStats::Text { added, removed }) => counts
            .child(
                div()
                    .text_color(colors.diff_added_text)
                    .child(format!("+{added}")),
            )
            .child(
                div()
                    .text_color(colors.diff_removed_text)
                    .child(format!("−{removed}")),
            ),
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
            .child(self.render_header(cx))
            .child(self.render_filter(window, cx))
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
