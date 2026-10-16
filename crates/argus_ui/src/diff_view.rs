//! The unified diff viewer: every file of the diff in one virtualized,
//! scrollable list (spec §4, §9, §12).
//!
//! # Rows
//!
//! Each file is a block of uniform-height rows ([`argus_diff::layout::Row`]):
//! its header, then hunk headers, lines and context expanders. Rows of all
//! files form one flat virtual index (`starts[file] + row`), so a
//! [`uniform_list`] lays out only the rows on screen and the scroll position
//! is plain arithmetic.
//!
//! # Lazy loading
//!
//! Files that were not diffed yet reserve [`layout::estimate_rows`]
//! placeholder rows (from the streamed +/- counts). When a file comes within
//! [`PREFETCH_SCREENS`] screens of the viewport its hunks are requested from
//! the [`DiffSession`] and laid out on the tokio runtime; a load whose file
//! leaves that range is cancelled. Every change of row counts keeps the row
//! under the top edge of the viewport where it is (see [`Anchor`]), so files
//! resolving above the viewport do not move the text being read.
//!
//! Loaded files are kept within [`VIEW_MEMORY_BUDGET`] by evicting the least
//! recently shown ones. An evicted file keeps its exact row count and is
//! reloaded (from the session's own LRU cache, or recomputed) when it comes
//! back near the viewport.
//!
//! # Rendering
//!
//! The code font is monospace, so lines are cut into display-column windows
//! before shaping: a horizontally scrolled line only shapes the visible
//! columns, and soft wrap splits a line into fixed-width segments, each one
//! row. Word-level highlights come from the diff engine and are computed off
//! the UI thread after a file loads; syntax highlights (#11) will join the
//! same text runs. Nothing here blocks on either.
//!
//! # Selection
//!
//! A click on the gutter selects a line; shift-click and dragging extend the
//! selection within the file. On release the selection is resolved into a
//! [`LineSelection`] (file, side, start, end) and emitted as
//! [`DiffViewEvent::LinesSelected`], or rejected with a hint (e.g. when it
//! crosses a hunk header). The gutter "+" emits
//! [`DiffViewEvent::CommentRequested`]; comments (#14) subscribe to both.

use std::collections::{HashMap, HashSet};
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use argus_diff::layout::{self, DisplayLine, Expand, FileRows, GapExpansion, LayoutOptions, Row};
use argus_diff::{
    ChangeKind, DiffContent, DiffSession, FileDiff, FileEntry, FileList, FileStats, LineKind,
    LoadMode, Side as DiffSide, StatsEvent,
};
use gpui::{
    AnyElement, Context, DispatchPhase, EventEmitter, FontWeight, HighlightStyle, Hsla,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollWheelEvent,
    SharedString, StyledText, Task, UniformListScrollHandle, Window, canvas, div, point,
    prelude::*, px, uniform_list,
};

use crate::components::{button, placeholder, toggle_button};
use crate::jobs::BackgroundJobs;
use crate::keymap::ToggleSoftWrap;
use crate::runtime::spawn_job;
use crate::session::DiffTarget;
use crate::theme::ActiveTheme;

/// Files within this many screens above and below the viewport are loaded.
const PREFETCH_SCREENS: usize = 2;
/// Memory the view may hold in loaded diffs and their rows before it evicts
/// the least recently shown files.
const VIEW_MEMORY_BUDGET: usize = 160 * 1024 * 1024;
/// Width of the vertical scrollbar track.
const SCROLLBAR_WIDTH: Pixels = px(12.0);
/// Smallest scrollbar thumb, so it stays grabbable on huge diffs.
const MIN_THUMB_HEIGHT: f32 = 24.0;
/// Soft wrap never goes narrower than this many columns.
const MIN_WRAP_COLUMNS: u32 = 20;
/// Height of the toolbar above the rows.
const TOOLBAR_HEIGHT: Pixels = px(32.0);
/// Width of the column holding the hover "+" comment button.
const COMMENT_COLUMN_WIDTH: Pixels = px(20.0);
/// Padding around a line-number cell.
const NUMBER_PADDING: Pixels = px(12.0);
/// Padding before the code text.
const CODE_PADDING: Pixels = px(4.0);
/// Group name shared by the elements of one line row, for hover effects.
const LINE_GROUP: &str = "diff-line";

/// A resolved line selection: what a comment anchors to (spec §7.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineSelection {
    /// File path in head (the merge-base path for deleted files).
    pub file: String,
    /// Path in the merge-base when the file was renamed.
    pub old_file: Option<String>,
    /// Old for removed lines, new otherwise.
    pub side: DiffSide,
    /// First line, 1-based, on `side`.
    pub start: u32,
    /// Last line, 1-based and inclusive, on `side`.
    pub end: u32,
}

/// Events of the diff view.
#[derive(Clone, Debug)]
pub enum DiffViewEvent {
    /// The user selected a line or a range in the gutter.
    LinesSelected(LineSelection),
    /// The gutter selection cannot be anchored; the text is the hint shown.
    SelectionRejected(SharedString),
    /// The user clicked the gutter "+" to comment on a line or range.
    CommentRequested(LineSelection),
}

/// How lines are displayed; persisted in the UI settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayOptions {
    /// Tab width in columns.
    pub tab_width: u32,
    /// Soft-wrap long lines instead of scrolling horizontally.
    pub soft_wrap: bool,
}

/// The file list of the diff.
enum ListStatus {
    Loading,
    Ready(Arc<FileList>),
    Failed(SharedString),
}

/// A file whose hunks are in memory, with its rows.
struct LoadedFile {
    diff: Arc<FileDiff>,
    rows: FileRows,
    layout: LayoutOptions,
    bytes: usize,
}

impl LoadedFile {
    fn new(diff: Arc<FileDiff>, rows: FileRows, layout: LayoutOptions) -> Self {
        let bytes = diff.memory_size() + rows.memory_size();
        Self {
            diff,
            rows,
            layout,
            bytes,
        }
    }
}

/// Where a file is in its load cycle.
enum FileState {
    /// Never loaded: placeholder rows estimated from the +/- counts.
    Pending,
    /// Hunks in memory.
    Loaded(Box<LoadedFile>),
    /// Unloaded to save memory; keeps the exact row count of its last
    /// layout so scrolling past it does not jump.
    Evicted,
    /// The diff could not be computed.
    Failed(SharedString),
}

/// Per-file view state.
struct FileSlot {
    stats: Option<FileStats>,
    state: FileState,
    /// Rows when expanded (header included).
    full_len: usize,
    collapsed: bool,
    viewed: bool,
    expansions: Vec<GapExpansion>,
    mode: LoadMode,
    last_used: u64,
    /// Computes the word-level highlights of a loaded file.
    words_task: Option<Task<()>>,
}

impl FileSlot {
    fn new(stats: Option<FileStats>, viewed: bool) -> Self {
        Self {
            full_len: layout::estimate_rows(stats.as_ref()),
            stats,
            state: FileState::Pending,
            collapsed: viewed,
            viewed,
            expansions: Vec::new(),
            mode: LoadMode::Normal,
            last_used: 0,
            words_task: None,
        }
    }

    /// Rows the file takes in the list.
    fn len(&self) -> usize {
        if self.collapsed { 1 } else { self.full_len }
    }

    fn loaded(&self) -> Option<&LoadedFile> {
        match &self.state {
            FileState::Loaded(loaded) => Some(loaded),
            _ => None,
        }
    }
}

/// A gutter selection in progress, as rows of one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    file: usize,
    anchor: usize,
    head: usize,
}

impl Selection {
    fn rows(&self) -> RangeInclusive<usize> {
        self.anchor.min(self.head)..=self.anchor.max(self.head)
    }
}

/// The row under the top edge of the viewport, as (file, row in file,
/// pixels into the row). Restored after row counts change.
#[derive(Clone, Copy, Debug)]
struct Anchor {
    file: usize,
    row: usize,
    offset: f64,
}

/// A mouse drag in progress.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    /// Extending a gutter selection.
    Gutter,
    /// Moving the scrollbar thumb, grabbed `grab` pixels below its top.
    Scrollbar { grab: f32 },
}

/// Font-derived sizes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Metrics {
    row_height: Pixels,
    char_width: Pixels,
}

/// The scrollbar's position, from the last layout.
struct ScrollbarGeometry {
    track_top: f32,
    track_height: f32,
    thumb_top: f32,
    thumb_height: f32,
    max_scroll: f32,
}

/// Horizontal measures of a row.
#[derive(Clone, Copy)]
struct Columns {
    number: Pixels,
    gutter: Pixels,
    sign: Pixels,
    code: Pixels,
}

/// The unified diff of one [`DiffTarget`].
pub struct DiffView {
    session: DiffSession,
    target: DiffTarget,
    status: ListStatus,
    files: Vec<FileSlot>,
    /// `starts[i]` is the first row of file `i`; `starts[files.len()]` is
    /// the total row count.
    starts: Vec<usize>,
    scroll: UniformListScrollHandle,
    h_offset: Pixels,
    options: DisplayOptions,
    wrap_columns: Option<u32>,
    metrics: Metrics,
    gutter_digits: u32,
    max_columns: u32,
    totals: (u64, u64),
    tick: u64,
    held_bytes: usize,
    loads: HashMap<usize, Task<()>>,
    selection: Option<Selection>,
    drag: Option<Drag>,
    hint: Option<SharedString>,
    remembered_viewed: HashSet<String>,
    list_task: Option<Task<()>>,
    stats_task: Option<Task<()>>,
}

impl EventEmitter<DiffViewEvent> for DiffView {}

impl DiffView {
    /// Opens the diff of `session` and starts loading its file list.
    /// `viewed` restores the "Viewed" marks of a previous view of the same
    /// branches.
    pub fn new(
        session: DiffSession,
        target: DiffTarget,
        options: DisplayOptions,
        viewed: HashSet<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            session,
            target,
            status: ListStatus::Loading,
            files: Vec::new(),
            starts: vec![0],
            scroll: UniformListScrollHandle::new(),
            h_offset: Pixels::ZERO,
            options,
            wrap_columns: None,
            metrics: Metrics {
                row_height: px(20.0),
                char_width: px(8.0),
            },
            gutter_digits: 4,
            max_columns: 0,
            totals: (0, 0),
            tick: 0,
            held_bytes: 0,
            loads: HashMap::new(),
            selection: None,
            drag: None,
            hint: None,
            remembered_viewed: viewed,
            list_task: None,
            stats_task: None,
        };
        this.load_list(cx);
        this
    }

    /// The target this view diffs.
    pub fn target(&self) -> &DiffTarget {
        &self.target
    }

    /// Paths marked "Viewed", to carry over to a refreshed view.
    pub fn viewed_paths(&self) -> HashSet<String> {
        let ListStatus::Ready(list) = &self.status else {
            return self.remembered_viewed.clone();
        };
        list.entries()
            .iter()
            .zip(&self.files)
            .filter(|(_, slot)| slot.viewed)
            .map(|(entry, _)| entry.path.clone())
            .collect()
    }

    /// Applies new display options (tab width, soft wrap).
    pub fn set_options(&mut self, options: DisplayOptions, cx: &mut Context<Self>) {
        if self.options == options {
            return;
        }
        self.options = options;
        if options.soft_wrap {
            self.h_offset = Pixels::ZERO;
        }
        // Soft wrap columns follow the width, measured at the next render.
        self.wrap_columns = None;
        self.relayout_loaded();
        cx.notify();
    }

    // ---- loading ------------------------------------------------------------

    fn load_list(&mut self, cx: &mut Context<Self>) {
        let session = self.session.clone();
        let job = spawn_job(cx, async move { session.file_list().await });
        BackgroundJobs::update(cx, |jobs| jobs.diffing = true);
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                BackgroundJobs::update(cx, |jobs| jobs.diffing = false);
                match result {
                    Ok(Ok(list)) => this.list_loaded(list, cx),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "failed to list the changed files");
                        this.status = ListStatus::Failed(error.to_string().into());
                    }
                    Err(error) => this.status = ListStatus::Failed(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn list_loaded(&mut self, list: Arc<FileList>, cx: &mut Context<Self>) {
        tracing::debug!(files = list.len(), "diff view file list loaded");
        let viewed = std::mem::take(&mut self.remembered_viewed);
        self.files = list
            .entries()
            .iter()
            .map(|entry| {
                FileSlot::new(
                    self.session.stats(entry.index),
                    viewed.contains(&entry.path),
                )
            })
            .collect();
        self.status = ListStatus::Ready(list);
        self.rebuild_starts();
        self.recount_totals();

        let mut updates = self.session.stats_updates();
        self.stats_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = updates.recv().await {
                let StatsEvent::Batch(batch) = event else {
                    continue;
                };
                let applied = this.update(cx, |this, cx| this.apply_stats(batch, cx));
                if applied.is_err() {
                    break;
                }
            }
        }));
    }

    fn apply_stats(&mut self, batch: Vec<(usize, FileStats)>, cx: &mut Context<Self>) {
        self.relayout(|this| {
            for (index, stats) in batch {
                let Some(slot) = this.files.get_mut(index) else {
                    continue;
                };
                if matches!(slot.state, FileState::Pending) {
                    slot.full_len = layout::estimate_rows(Some(&stats));
                }
                slot.stats = Some(stats);
            }
        });
        self.recount_totals();
        cx.notify();
    }

    fn recount_totals(&mut self) {
        self.totals = self
            .files
            .iter()
            .fold((0, 0), |(added, removed), slot| match slot.stats {
                Some(FileStats::Text {
                    added: a,
                    removed: r,
                }) => (added + u64::from(a), removed + u64::from(r)),
                _ => (added, removed),
            });
    }

    fn layout_options(&self) -> LayoutOptions {
        LayoutOptions {
            tab_width: self.options.tab_width,
            wrap_columns: self.wrap_columns.filter(|_| self.options.soft_wrap),
        }
    }

    /// Requests the hunks of `file` and lays them out on the runtime.
    /// Dropping the task (file scrolled away, view closed) cancels it.
    fn start_load(&mut self, file: usize, cx: &mut Context<Self>) {
        let Some(slot) = self.files.get(file) else {
            return;
        };
        let session = self.session.clone();
        let (mode, expansions, layout) =
            (slot.mode, slot.expansions.clone(), self.layout_options());
        let job = spawn_job(cx, async move {
            let diff = session
                .file_diff_at(file, mode)
                .await
                .map_err(|error| error.to_string())?;
            let rows = {
                let diff = diff.clone();
                tokio::task::spawn_blocking(move || layout::build_rows(&diff, &expansions, layout))
                    .await
                    .map_err(|error| error.to_string())?
            };
            Ok::<_, String>((diff, rows))
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|error| Err(error.to_string()));
            this.update(cx, |this, cx| this.finish_load(file, layout, result, cx))
                .ok();
        });
        self.loads.insert(file, task);
    }

    fn finish_load(
        &mut self,
        file: usize,
        layout_used: LayoutOptions,
        result: Result<(Arc<FileDiff>, FileRows), String>,
        cx: &mut Context<Self>,
    ) {
        self.loads.remove(&file);
        if file >= self.files.len() {
            return;
        }
        match result {
            Ok((diff, rows)) => {
                let layout = self.layout_options();
                let rows = if layout_used == layout {
                    rows
                } else {
                    layout::build_rows(&diff, &self.files[file].expansions, layout)
                };
                self.gutter_digits = self.gutter_digits.max(rows.line_digits);
                self.max_columns = self.max_columns.max(rows.max_columns);
                let loaded = LoadedFile::new(diff.clone(), rows, layout);
                self.held_bytes += loaded.bytes;
                let tick = self.tick;
                self.relayout(|this| {
                    let slot = &mut this.files[file];
                    slot.full_len = loaded.rows.rows.len();
                    slot.last_used = tick;
                    if let Some(stats) = loaded.diff.stats() {
                        slot.stats = Some(stats);
                    }
                    slot.state = FileState::Loaded(Box::new(loaded));
                });
                self.compute_words(file, diff, cx);
            }
            Err(message) => {
                tracing::warn!(file, %message, "failed to load a file diff");
                self.relayout(|this| {
                    let slot = &mut this.files[file];
                    slot.state = FileState::Failed(message.into());
                    slot.full_len = 2;
                });
            }
        }
        cx.notify();
    }

    /// Computes the word-level highlights of every hunk of `file` off the UI
    /// thread, one hunk at a time, and repaints when done.
    fn compute_words(&mut self, file: usize, diff: Arc<FileDiff>, cx: &mut Context<Self>) {
        let has_words = diff.text().is_some_and(|text| text.has_word_diff());
        if !has_words {
            return;
        }
        let job = spawn_job(cx, async move {
            let hunks = diff.text().map_or(0, |text| text.hunks().len());
            for hunk in 0..hunks {
                let diff = diff.clone();
                let computed = tokio::task::spawn_blocking(move || {
                    if let Some(text) = diff.text() {
                        text.intra_line(hunk);
                    }
                })
                .await;
                if computed.is_err() {
                    return;
                }
            }
        });
        self.files[file].words_task = Some(cx.spawn(async move |this, cx| {
            if job.await.is_ok() {
                this.update(cx, |_, cx| cx.notify()).ok();
            }
        }));
    }

    /// Loads files near the viewport, cancels loads that left it, and evicts
    /// what the memory budget cannot hold.
    fn prefetch(&mut self, viewport_height: Pixels, cx: &mut Context<Self>) {
        let total = self.total_rows();
        if total == 0 {
            return;
        }
        let screen = (viewport_height / self.metrics.row_height).ceil().max(1.0) as usize + 1;
        // While the scrollbar is dragged, only what is on screen: loads for
        // files flown over would be cancelled a frame later anyway.
        let margin = if matches!(self.drag, Some(Drag::Scrollbar { .. })) {
            0
        } else {
            screen * PREFETCH_SCREENS
        };
        let top = self.top_row();
        let first = self.file_at(top.saturating_sub(margin));
        let last = self.file_at((top + screen + margin).min(total - 1));
        self.tick += 1;
        for file in first..=last {
            let slot = &mut self.files[file];
            slot.last_used = self.tick;
            let needs_load = matches!(slot.state, FileState::Pending | FileState::Evicted)
                && !slot.collapsed
                && !self.loads.contains_key(&file);
            if needs_load {
                self.start_load(file, cx);
            }
        }
        self.loads.retain(|file, _| (first..=last).contains(file));
        self.evict(first..=last);
    }

    /// Drops the least recently shown loaded files outside `keep` until the
    /// view fits its memory budget. Row counts are kept.
    fn evict(&mut self, keep: RangeInclusive<usize>) {
        while self.held_bytes > VIEW_MEMORY_BUDGET {
            let victim = self
                .files
                .iter()
                .enumerate()
                .filter(|(index, slot)| {
                    !keep.contains(index) && matches!(slot.state, FileState::Loaded(_))
                })
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(index, _)| index);
            let Some(victim) = victim else {
                break;
            };
            let slot = &mut self.files[victim];
            if let FileState::Loaded(loaded) =
                std::mem::replace(&mut slot.state, FileState::Evicted)
            {
                self.held_bytes = self.held_bytes.saturating_sub(loaded.bytes);
            }
            slot.words_task = None;
            if self
                .selection
                .is_some_and(|selection| selection.file == victim)
            {
                self.selection = None;
            }
            tracing::trace!(file = victim, "evicted a file diff");
        }
    }

    /// Lays loaded files out again after the layout options changed.
    fn relayout_loaded(&mut self) {
        let layout = self.layout_options();
        self.relayout(|this| {
            for slot in &mut this.files {
                let FileState::Loaded(loaded) = &mut slot.state else {
                    continue;
                };
                if loaded.layout == layout {
                    continue;
                }
                let rows = layout::build_rows(&loaded.diff, &slot.expansions, layout);
                **loaded = LoadedFile::new(loaded.diff.clone(), rows, layout);
                slot.full_len = loaded.rows.rows.len();
            }
            this.selection = None;
        });
        self.held_bytes = self
            .files
            .iter()
            .filter_map(FileSlot::loaded)
            .map(|loaded| loaded.bytes)
            .sum();
        self.max_columns = self
            .files
            .iter()
            .filter_map(FileSlot::loaded)
            .map(|loaded| loaded.rows.max_columns)
            .max()
            .unwrap_or(0);
    }

    // ---- row index and scrolling ------------------------------------------

    fn rebuild_starts(&mut self) {
        self.starts.clear();
        self.starts.reserve(self.files.len() + 1);
        let mut at = 0;
        self.starts.push(0);
        for slot in &self.files {
            at += slot.len();
            self.starts.push(at);
        }
    }

    fn total_rows(&self) -> usize {
        self.starts.last().copied().unwrap_or(0)
    }

    /// The file owning global row `row` (clamped to the last file).
    fn file_at(&self, row: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1)
            .min(self.files.len().saturating_sub(1))
    }

    fn scroll_top(&self) -> f64 {
        -f64::from(self.scroll.0.borrow().base_handle.offset().y.as_f32())
    }

    fn set_scroll_top(&self, top: f64) {
        let handle = self.scroll.0.borrow();
        let offset = handle.base_handle.offset();
        handle
            .base_handle
            .set_offset(point(offset.x, px(-(top.max(0.0) as f32))));
    }

    fn row_height(&self) -> f64 {
        f64::from(self.metrics.row_height.as_f32())
    }

    fn top_row(&self) -> usize {
        let row = (self.scroll_top() / self.row_height()).floor().max(0.0) as usize;
        row.min(self.total_rows().saturating_sub(1))
    }

    fn list_bounds(&self) -> gpui::Bounds<Pixels> {
        self.scroll.0.borrow().base_handle.bounds()
    }

    fn anchor(&self) -> Option<Anchor> {
        if self.total_rows() == 0 {
            return None;
        }
        let row = self.top_row();
        let file = self.file_at(row);
        Some(Anchor {
            file,
            row: row - self.starts[file],
            offset: self.scroll_top() - row as f64 * self.row_height(),
        })
    }

    fn restore(&self, anchor: Anchor) {
        let Some(slot) = self.files.get(anchor.file) else {
            return;
        };
        let row = self.starts[anchor.file] + anchor.row.min(slot.len().saturating_sub(1));
        self.set_scroll_top(row as f64 * self.row_height() + anchor.offset);
    }

    /// Runs `change` (which may change row counts) keeping the row at the
    /// top of the viewport in place.
    fn relayout(&mut self, change: impl FnOnce(&mut Self)) {
        let anchor = self.anchor();
        change(self);
        self.rebuild_starts();
        if let Some(anchor) = anchor {
            self.restore(anchor);
        }
    }

    /// Scrolls so that global row `row` is the first one in the viewport.
    fn scroll_to_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let viewport = f64::from(self.list_bounds().size.height.as_f32());
        let content = self.total_rows() as f64 * self.row_height();
        let max = (content - viewport).max(0.0);
        self.set_scroll_top((row as f64 * self.row_height()).min(max));
        cx.notify();
    }

    fn scrollbar(&self) -> Option<ScrollbarGeometry> {
        let bounds = self.list_bounds();
        let track_height = bounds.size.height.as_f32();
        let content = (self.total_rows() as f64 * self.row_height()) as f32;
        if track_height <= 0.0 || content <= track_height {
            return None;
        }
        let thumb_height = (track_height * track_height / content)
            .max(MIN_THUMB_HEIGHT)
            .min(track_height);
        let max_scroll = content - track_height;
        let fraction = (self.scroll_top() as f32 / max_scroll).clamp(0.0, 1.0);
        Some(ScrollbarGeometry {
            track_top: bounds.origin.y.as_f32(),
            track_height,
            thumb_top: fraction * (track_height - thumb_height),
            thumb_height,
            max_scroll,
        })
    }

    fn move_thumb(&mut self, thumb_top: f32, cx: &mut Context<Self>) {
        let Some(geometry) = self.scrollbar() else {
            return;
        };
        let travel = (geometry.track_height - geometry.thumb_height).max(1.0);
        let fraction = (thumb_top / travel).clamp(0.0, 1.0);
        self.set_scroll_top(f64::from(fraction * geometry.max_scroll));
        cx.notify();
    }

    fn scrollbar_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        let Some(geometry) = self.scrollbar() else {
            return;
        };
        let y = event.position.y.as_f32() - geometry.track_top;
        let on_thumb =
            (geometry.thumb_top..=geometry.thumb_top + geometry.thumb_height).contains(&y);
        let grab = if on_thumb {
            y - geometry.thumb_top
        } else {
            geometry.thumb_height / 2.0
        };
        self.drag = Some(Drag::Scrollbar { grab });
        self.move_thumb(y - grab, cx);
    }

    fn scroll_horizontally(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        if self.options.soft_wrap {
            return;
        }
        let delta = event.delta.pixel_delta(self.metrics.row_height);
        if delta.x.abs() <= delta.y.abs() {
            return;
        }
        let columns = self.columns();
        let content = self.metrics.char_width * (self.max_columns + 2) as f32;
        let max = (content - columns.code).max(Pixels::ZERO);
        let next = (self.h_offset - delta.x).clamp(Pixels::ZERO, max);
        if next != self.h_offset {
            self.h_offset = next;
            cx.notify();
        }
    }

    /// The global row under window y coordinate `y`.
    fn row_at(&self, y: Pixels) -> usize {
        let bounds = self.list_bounds();
        let local = f64::from((y - bounds.origin.y).as_f32()) + self.scroll_top();
        let row = (local / self.row_height()).floor().max(0.0) as usize;
        row.min(self.total_rows().saturating_sub(1))
    }

    // ---- navigation ---------------------------------------------------------

    /// Rows (within the file) of the hunk headers of `file`. Unloaded files
    /// report their first body row, where their first hunk will be.
    fn hunk_rows(&self, file: usize) -> Vec<usize> {
        let slot = &self.files[file];
        if slot.collapsed {
            return Vec::new();
        }
        match &slot.state {
            FileState::Loaded(loaded) => loaded
                .rows
                .hunk_rows
                .iter()
                .map(|&row| row as usize)
                .collect(),
            FileState::Pending | FileState::Evicted if slot.full_len > 1 => vec![1],
            _ => Vec::new(),
        }
    }

    /// Scrolls the next file's header to the top (`j`).
    pub fn next_file(&mut self, cx: &mut Context<Self>) {
        if self.files.is_empty() {
            return;
        }
        let current = self.file_at(self.top_row());
        if current + 1 < self.files.len() {
            self.scroll_to_row(self.starts[current + 1], cx);
        }
    }

    /// Scrolls to the current file's header, or the previous file's when
    /// already there (`k`).
    pub fn previous_file(&mut self, cx: &mut Context<Self>) {
        if self.files.is_empty() {
            return;
        }
        let top = self.top_row();
        let current = self.file_at(top);
        let at_header = top == self.starts[current] && self.scroll_top() % self.row_height() == 0.0;
        let target = if at_header {
            current.saturating_sub(1)
        } else {
            current
        };
        self.scroll_to_row(self.starts[target], cx);
    }

    /// Scrolls the next hunk header just below the sticky file header (`n`).
    pub fn next_hunk(&mut self, cx: &mut Context<Self>) {
        if self.files.is_empty() {
            return;
        }
        let below_header = self.top_row() + 1;
        for file in self.file_at(self.top_row())..self.files.len() {
            let start = self.starts[file];
            if let Some(row) = self
                .hunk_rows(file)
                .into_iter()
                .map(|row| start + row)
                .find(|&row| row > below_header)
            {
                self.scroll_to_row(row - 1, cx);
                return;
            }
        }
    }

    /// Scrolls the previous hunk header just below the sticky file header
    /// (`p`).
    pub fn previous_hunk(&mut self, cx: &mut Context<Self>) {
        if self.files.is_empty() {
            return;
        }
        let below_header = self.top_row() + 1;
        for file in (0..=self.file_at(self.top_row())).rev() {
            let start = self.starts[file];
            if let Some(row) = self
                .hunk_rows(file)
                .into_iter()
                .rev()
                .map(|row| start + row)
                .find(|&row| row < below_header)
            {
                self.scroll_to_row(row - 1, cx);
                return;
            }
        }
    }

    // ---- file actions -------------------------------------------------------

    fn toggle_collapsed(&mut self, file: usize, cx: &mut Context<Self>) {
        self.relayout(|this| {
            let slot = &mut this.files[file];
            slot.collapsed = !slot.collapsed;
        });
        cx.notify();
    }

    fn toggle_viewed(&mut self, file: usize, cx: &mut Context<Self>) {
        self.relayout(|this| {
            let slot = &mut this.files[file];
            slot.viewed = !slot.viewed;
            // Like GitHub: viewed files fold away, un-viewing unfolds.
            slot.collapsed = slot.viewed;
        });
        cx.notify();
    }

    fn expand_gap(&mut self, file: usize, gap: usize, how: Expand, cx: &mut Context<Self>) {
        let layout = self.layout_options();
        let mut held = self.held_bytes;
        self.relayout(|this| {
            let slot = &mut this.files[file];
            let FileState::Loaded(loaded) = &mut slot.state else {
                return;
            };
            let Some(gap_lines) = loaded.rows.gaps.get(gap) else {
                return;
            };
            if slot.expansions.len() < loaded.rows.gaps.len() {
                slot.expansions
                    .resize(loaded.rows.gaps.len(), GapExpansion::default());
            }
            slot.expansions[gap] = layout::expand(gap_lines, slot.expansions[gap], how);
            let rows = layout::build_rows(&loaded.diff, &slot.expansions, layout);
            held = held.saturating_sub(loaded.bytes);
            **loaded = LoadedFile::new(loaded.diff.clone(), rows, layout);
            held += loaded.bytes;
            slot.full_len = loaded.rows.rows.len();
            if this
                .selection
                .is_some_and(|selection| selection.file == file)
            {
                this.selection = None;
            }
        });
        self.held_bytes = held;
        cx.notify();
    }

    /// "Load anyway" for a large file, or "Retry" after a failure.
    fn force_load(&mut self, file: usize, mode: LoadMode, cx: &mut Context<Self>) {
        let slot = &mut self.files[file];
        slot.mode = mode;
        if let FileState::Loaded(loaded) = std::mem::replace(&mut slot.state, FileState::Pending) {
            self.held_bytes = self.held_bytes.saturating_sub(loaded.bytes);
        }
        self.loads.remove(&file);
        self.start_load(file, cx);
        cx.notify();
    }

    // ---- selection ----------------------------------------------------------

    fn gutter_down(&mut self, file: usize, row: usize, shift: bool, cx: &mut Context<Self>) {
        self.selection = match self.selection {
            Some(selection) if shift && selection.file == file => Some(Selection {
                head: row,
                ..selection
            }),
            _ => Some(Selection {
                file,
                anchor: row,
                head: row,
            }),
        };
        self.drag = Some(Drag::Gutter);
        cx.notify();
    }

    fn drag_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag_end(cx);
            return;
        }
        match self.drag {
            Some(Drag::Scrollbar { grab }) => {
                if let Some(geometry) = self.scrollbar() {
                    let y = event.position.y.as_f32() - geometry.track_top;
                    self.move_thumb(y - grab, cx);
                }
            }
            Some(Drag::Gutter) => {
                let Some(selection) = self.selection else {
                    return;
                };
                let row = self.row_at(event.position.y);
                let start = self.starts[selection.file];
                let len = self.files[selection.file].len();
                // Stay within the file; headers and expanders in between are
                // caught when the selection is resolved.
                let head = row.clamp(start + 1, start + len.saturating_sub(1).max(1)) - start;
                if head != selection.head {
                    self.selection = Some(Selection { head, ..selection });
                    cx.notify();
                }
            }
            None => {}
        }
    }

    fn drag_end(&mut self, cx: &mut Context<Self>) {
        match self.drag.take() {
            Some(Drag::Gutter) => {
                if let Some(selection) = self.resolve_selection(cx) {
                    cx.emit(DiffViewEvent::LinesSelected(selection));
                }
            }
            Some(Drag::Scrollbar { .. }) => {}
            None => return,
        }
        cx.notify();
    }

    /// The current gutter selection as a line range: `None` without a
    /// selection, `Some(Err(_))` when it cannot be anchored.
    pub fn selection(&self) -> Option<Result<LineSelection, layout::RangeError>> {
        let selection = self.selection?;
        let ListStatus::Ready(list) = &self.status else {
            return None;
        };
        let entry = list.entries().get(selection.file)?;
        let loaded = self.files.get(selection.file)?.loaded()?;
        let text = loaded.diff.text()?;
        let range = layout::select_range(text, &loaded.rows.rows, selection.anchor, selection.head);
        Some(range.map(|range| LineSelection {
            file: entry.path.clone(),
            old_file: entry.old_path.clone(),
            side: range.side,
            start: range.start,
            end: range.end,
        }))
    }

    /// Resolves the gutter selection, or clears it and shows why it cannot
    /// be anchored.
    fn resolve_selection(&mut self, cx: &mut Context<Self>) -> Option<LineSelection> {
        match self.selection()? {
            Ok(selection) => {
                self.hint = None;
                Some(selection)
            }
            Err(error) => {
                let hint: SharedString = error.to_string().into();
                self.hint = Some(hint.clone());
                self.selection = None;
                cx.emit(DiffViewEvent::SelectionRejected(hint));
                None
            }
        }
    }

    /// The gutter "+": comments on the current selection when the row is in
    /// it, otherwise on that line alone.
    fn request_comment(&mut self, file: usize, row: usize, cx: &mut Context<Self>) {
        let inside = self
            .selection
            .is_some_and(|selection| selection.file == file && selection.rows().contains(&row));
        if !inside {
            self.selection = Some(Selection {
                file,
                anchor: row,
                head: row,
            });
        }
        if let Some(selection) = self.resolve_selection(cx) {
            cx.emit(DiffViewEvent::CommentRequested(selection));
        }
        cx.notify();
    }

    fn is_selected(&self, file: usize, row: usize) -> bool {
        self.selection
            .is_some_and(|selection| selection.file == file && selection.rows().contains(&row))
    }

    // ---- rendering ----------------------------------------------------------

    fn update_metrics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fonts = cx.theme().fonts.clone();
        let font_id = window
            .text_system()
            .resolve_font(&gpui::font(fonts.code_family.clone()));
        let char_width = window
            .text_system()
            .advance(font_id, fonts.code_size, 'm')
            .map(|size| size.width)
            .unwrap_or(fonts.code_size * 0.6);
        let metrics = Metrics {
            row_height: (fonts.code_size * 1.5).round(),
            char_width,
        };
        if metrics != self.metrics {
            let anchor = self.anchor();
            self.metrics = metrics;
            if let Some(anchor) = anchor {
                self.restore(anchor);
            }
        }
    }

    fn columns(&self) -> Columns {
        let char_width = self.metrics.char_width;
        let number = char_width * self.gutter_digits as f32 + NUMBER_PADDING;
        let gutter = number * 2.0;
        let sign = char_width * 2.0;
        let width = self.list_bounds().size.width;
        let code = (width - gutter - COMMENT_COLUMN_WIDTH - sign - CODE_PADDING - SCROLLBAR_WIDTH)
            .max(char_width * MIN_WRAP_COLUMNS as f32);
        Columns {
            number,
            gutter,
            sign,
            code,
        }
    }

    /// Re-wraps loaded files when the soft-wrap width changed.
    fn update_wrap(&mut self) {
        let wanted = if self.options.soft_wrap && self.list_bounds().size.width > Pixels::ZERO {
            let columns = (self.columns().code / self.metrics.char_width).floor() as u32;
            Some(columns.max(MIN_WRAP_COLUMNS))
        } else {
            None
        };
        if wanted != self.wrap_columns {
            self.wrap_columns = wanted;
            self.relayout_loaded();
        }
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let ListStatus::Ready(list) = &self.status else {
            return Vec::new();
        };
        let list = list.clone();
        let columns = self.columns();
        range
            .map(|row| {
                let file = self.file_at(row);
                let local = row - self.starts[file];
                let entry = &list.entries()[file];
                let element = if local == 0 {
                    self.render_file_header(file, entry, false, cx)
                } else {
                    self.render_body_row(file, entry, local, row, columns, cx)
                };
                div()
                    .h(self.metrics.row_height)
                    .w_full()
                    .overflow_hidden()
                    .child(element)
                    .into_any_element()
            })
            .collect()
    }

    fn render_body_row(
        &self,
        file: usize,
        entry: &FileEntry,
        local: usize,
        row: usize,
        columns: Columns,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let slot = &self.files[file];
        match &slot.state {
            FileState::Loaded(loaded) => match loaded.rows.rows.get(local) {
                Some(&Row::HunkHeader { hunk }) => {
                    let header = loaded
                        .diff
                        .text()
                        .and_then(|text| text.hunks().get(hunk as usize))
                        .map(|hunk| hunk.header())
                        .unwrap_or_default();
                    div()
                        .flex()
                        .size_full()
                        .bg(colors.accent.opacity(0.08))
                        .child(
                            div()
                                .flex_none()
                                .h_full()
                                .w(columns.gutter + COMMENT_COLUMN_WIDTH)
                                .bg(colors.accent.opacity(0.12)),
                        )
                        .child(
                            div()
                                .pl(columns.sign + CODE_PADDING)
                                .text_color(colors.text_muted)
                                .whitespace_nowrap()
                                .child(header),
                        )
                        .into_any_element()
                }
                Some(&Row::Expander { gap }) => {
                    self.render_expander(file, loaded, gap as usize, row, columns, cx)
                }
                Some(&Row::Notice) => self.render_notice(file, entry, slot, columns, cx),
                Some(&line_row @ (Row::Line { .. } | Row::Context { .. })) => {
                    self.render_line(file, loaded, line_row, local, row, columns, cx)
                }
                Some(Row::FileHeader | Row::Placeholder) | None => div().into_any_element(),
            },
            FileState::Failed(_) if local == 1 => {
                self.render_notice(file, entry, slot, columns, cx)
            }
            FileState::Failed(_) => div().into_any_element(),
            FileState::Pending | FileState::Evicted => div()
                .flex()
                .items_center()
                .size_full()
                .bg(colors.surface.opacity(0.35))
                .child(div().w(columns.gutter + COMMENT_COLUMN_WIDTH).h_full())
                .when(local == 1, |row| {
                    row.child(
                        div()
                            .pl(columns.sign + CODE_PADDING)
                            .text_color(colors.text_placeholder)
                            .child("Loading…"),
                    )
                })
                .into_any_element(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_line(
        &self,
        file: usize,
        loaded: &LoadedFile,
        row_kind: Row,
        local: usize,
        row: usize,
        columns: Columns,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let Some(text) = loaded.diff.text() else {
            return div().into_any_element();
        };
        let Some(line) = layout::row_line(text, row_kind) else {
            return div().into_any_element();
        };
        let (background, sign, sign_color, word_background) = match line.kind {
            LineKind::Added => (
                colors.diff_added_background,
                "+",
                colors.diff_added_text,
                colors.diff_added_word,
            ),
            LineKind::Removed => (
                colors.diff_removed_background,
                "-",
                colors.diff_removed_text,
                colors.diff_removed_word,
            ),
            LineKind::Context => (colors.background, " ", colors.text_muted, colors.background),
        };
        let first_segment = line.segment == 0;
        let selected = self.is_selected(file, local);
        let char_width = self.metrics.char_width;

        let window = match loaded.layout.wrap_columns {
            Some(width) => line.segment * width..(line.segment + 1) * width,
            None => {
                let first = (self.h_offset / char_width).floor() as u32;
                let visible = (columns.code / char_width).ceil() as u32 + 2;
                first..first + visible
            }
        };
        let shift = match loaded.layout.wrap_columns {
            Some(_) => Pixels::ZERO,
            None => self.h_offset - char_width * window.start as f32,
        };
        let display = layout::display_window(line.text, line.crlf, loaded.layout.tab_width, window);
        let words = line
            .diff_index
            .and_then(|index| {
                let hunk = text.hunk_of_line(index)?;
                Some(text.intra_line_if_ready(hunk)?.ranges(index))
            })
            .unwrap_or(&[]);
        let runs = highlight_runs(&display, words, word_background, colors.text_placeholder);
        let code = StyledText::new(display.text).with_highlights(runs);

        let number = |value: Option<u32>| {
            div()
                .flex_none()
                .w(columns.number)
                .pr(NUMBER_PADDING / 2.0)
                .text_right()
                .whitespace_nowrap()
                .children(value.filter(|_| first_segment).map(|n| n.to_string()))
        };
        let gutter_background = if selected {
            colors.accent.opacity(0.35)
        } else {
            colors.background.opacity(0.4)
        };
        let gutter = div()
            .id(("diff-gutter", row))
            .flex()
            .flex_none()
            .h_full()
            .w(columns.gutter)
            .bg(gutter_background)
            .text_color(if selected {
                colors.text
            } else {
                colors.text_placeholder
            })
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.gutter_down(file, local, event.modifiers.shift, cx);
                }),
            )
            .child(number(line.old_line))
            .child(number(line.new_line));
        let comment_button = div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w(COMMENT_COLUMN_WIDTH)
            .h_full()
            .when(first_segment, |column| {
                let accent = colors.accent;
                column.child(
                    div()
                        .id(("diff-comment", row))
                        .invisible()
                        .group_hover(LINE_GROUP, |style| style.visible())
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(16.0))
                        .rounded_sm()
                        .bg(accent)
                        .text_color(colors.accent_text)
                        .font_weight(FontWeight::BOLD)
                        .cursor_pointer()
                        .hover(move |style| style.bg(accent.opacity(0.85)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.request_comment(file, local, cx);
                            }),
                        )
                        .child("+"),
                )
            });
        div()
            .group(LINE_GROUP)
            .flex()
            .size_full()
            .bg(background)
            .child(gutter)
            .child(comment_button)
            .child(
                div()
                    .flex_none()
                    .w(columns.sign)
                    .text_color(sign_color)
                    .children(first_segment.then_some(sign)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .pl(CODE_PADDING)
                    .whitespace_nowrap()
                    .when(selected, |code| code.bg(colors.accent.opacity(0.1)))
                    .child(div().relative().left(-shift).child(code)),
            )
            .into_any_element()
    }

    fn render_expander(
        &self,
        file: usize,
        loaded: &LoadedFile,
        gap: usize,
        row: usize,
        columns: Columns,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let gaps = &loaded.rows.gaps;
        let Some(gap_lines) = gaps.get(gap) else {
            return div().into_any_element();
        };
        let expansion = self.files[file]
            .expansions
            .get(gap)
            .copied()
            .unwrap_or_default();
        let hidden = expansion.hidden(gap_lines);
        let (up, down) = layout::expand_directions(gap, gaps.len());
        let step = layout::EXPAND_STEP.min(hidden);
        let action = |id: &'static str, label: String, how: Expand| {
            div()
                .id((id, row))
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .text_color(colors.accent)
                .hover(|style| style.bg(colors.element_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.expand_gap(file, gap, how, cx)))
                .child(label)
        };
        let partial = hidden > layout::EXPAND_STEP;
        div()
            .flex()
            .items_center()
            .gap_2()
            .size_full()
            .bg(colors.accent.opacity(0.05))
            .child(
                div()
                    .flex_none()
                    .h_full()
                    .w(columns.gutter + COMMENT_COLUMN_WIDTH)
                    .bg(colors.accent.opacity(0.10)),
            )
            .child(div().w(columns.sign))
            .when(up && partial, |row| {
                row.child(action(
                    "diff-expand-up",
                    format!("↑ Expand {step}"),
                    Expand::Up,
                ))
            })
            .when(down && partial, |row| {
                row.child(action(
                    "diff-expand-down",
                    format!("↓ Expand {step}"),
                    Expand::Down,
                ))
            })
            .child(action(
                "diff-expand-all",
                format!(
                    "Expand all {hidden} hidden line{}",
                    if hidden == 1 { "" } else { "s" }
                ),
                Expand::All,
            ))
            .into_any_element()
    }

    fn render_notice(
        &self,
        file: usize,
        entry: &FileEntry,
        slot: &FileSlot,
        columns: Columns,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let (message, color, action): (String, Hsla, Option<(&'static str, LoadMode)>) =
            match &slot.state {
                FileState::Failed(message) => (
                    format!("Could not load the diff: {message}"),
                    colors.error,
                    Some(("Retry", slot.mode)),
                ),
                FileState::Loaded(loaded) => match &loaded.diff.content {
                    DiffContent::Binary { old_size, new_size } => (
                        format!(
                            "Binary file changed ({} → {}).",
                            format_size(*old_size),
                            format_size(*new_size)
                        ),
                        colors.text_muted,
                        None,
                    ),
                    DiffContent::TooLarge { old_size, new_size } => (
                        format!(
                            "Large file, diff not shown ({}).",
                            format_size((*old_size).max(*new_size))
                        ),
                        colors.warning,
                        Some(("Load anyway", LoadMode::Force)),
                    ),
                    DiffContent::Text(_) => (no_hunks_message(entry), colors.text_muted, None),
                },
                _ => (String::new(), colors.text_muted, None),
            };
        div()
            .flex()
            .items_center()
            .gap_2()
            .size_full()
            .child(
                div()
                    .flex_none()
                    .w(columns.gutter + COMMENT_COLUMN_WIDTH + columns.sign),
            )
            .child(div().text_color(color).whitespace_nowrap().child(message))
            .children(action.map(|(label, mode)| {
                button(("diff-notice-action", file), label, cx)
                    .h(self.metrics.row_height - px(2.0))
                    .on_click(cx.listener(move |this, _, _, cx| this.force_load(file, mode, cx)))
            }))
            .into_any_element()
    }

    fn render_file_header(
        &self,
        file: usize,
        entry: &FileEntry,
        sticky: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let slot = &self.files[file];
        let id_base = if sticky { "diff-sticky" } else { "diff-header" };
        let (badge, badge_color) = match entry.kind {
            ChangeKind::Added => ("A", colors.diff_added_text),
            ChangeKind::Deleted => ("D", colors.diff_removed_text),
            ChangeKind::Modified => ("M", colors.warning),
            ChangeKind::Renamed => ("R", colors.accent),
            ChangeKind::ModeChanged => ("M", colors.text_muted),
            ChangeKind::TypeChanged => ("T", colors.warning),
        };
        let path = match &entry.old_path {
            Some(old) => format!("{old} → {}", entry.path),
            None => entry.path.clone(),
        };
        let counts: AnyElement = match &slot.stats {
            Some(FileStats::Text { added, removed }) => div()
                .flex()
                .gap_1()
                .child(
                    div()
                        .text_color(colors.diff_added_text)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .text_color(colors.diff_removed_text)
                        .child(format!("−{removed}")),
                )
                .into_any_element(),
            Some(FileStats::Binary { .. }) => div()
                .text_color(colors.text_muted)
                .child("binary")
                .into_any_element(),
            Some(FileStats::TooLarge { .. }) => div()
                .text_color(colors.text_muted)
                .child("large")
                .into_any_element(),
            Some(FileStats::Failed(_)) => div()
                .text_color(colors.error)
                .child("error")
                .into_any_element(),
            None => div()
                .text_color(colors.text_placeholder)
                .child("…")
                .into_any_element(),
        };
        let mode = entry.mode_change().map(|(old, new)| {
            div()
                .flex_none()
                .text_color(colors.text_muted)
                .child(format!("{} → {}", old.mode(), new.mode()))
        });
        let viewed = slot.viewed;
        div()
            .flex()
            .items_center()
            .gap_2()
            .size_full()
            .px_1()
            .bg(colors.surface)
            .border_b_1()
            .border_color(colors.border_variant)
            .when(file > 0 && !sticky, |header| {
                header.border_t_1().border_color(colors.border)
            })
            // Keep the controls clear of the scrollbar; the sticky copy
            // already ends before it.
            .when(!sticky, |header| header.pr(SCROLLBAR_WIDTH + px(4.0)))
            .font_family(theme.fonts.ui_family.clone())
            .text_size(theme.fonts.ui_size * 0.9)
            .child(
                div()
                    .id((id_base, file * 3))
                    .flex_none()
                    .w(px(18.0))
                    .text_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(colors.text_muted)
                    .hover(|style| style.bg(colors.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_collapsed(file, cx)))
                    .child(if slot.collapsed { "▸" } else { "▾" }),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::BOLD)
                    .text_color(badge_color)
                    .child(badge),
            )
            .child(
                div()
                    .id((id_base, file * 3 + 1))
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_family(theme.fonts.code_family.clone())
                    .text_color(colors.text)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_collapsed(file, cx)))
                    .child(path),
            )
            .child(div().flex_none().child(counts))
            .children(mode)
            .child(div().flex_1())
            .child(
                div()
                    .id((id_base, file * 3 + 2))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(if viewed {
                        colors.text
                    } else {
                        colors.text_muted
                    })
                    .hover(|style| style.bg(colors.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_viewed(file, cx)))
                    .child(if viewed { "☑" } else { "☐" })
                    .child("Viewed"),
            )
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let summary: SharedString = match &self.status {
            ListStatus::Loading => "Listing changed files…".into(),
            ListStatus::Failed(_) => "No diff".into(),
            ListStatus::Ready(list) => {
                let files = list.len();
                format!("{files} file{} changed", if files == 1 { "" } else { "s" },).into()
            }
        };
        let (added, removed) = self.totals;
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap_2()
            .h(TOOLBAR_HEIGHT)
            .px_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .text_sm()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(div().text_color(colors.text_muted).child(summary))
                    .when(matches!(self.status, ListStatus::Ready(_)), |row| {
                        row.child(
                            div()
                                .text_color(colors.diff_added_text)
                                .child(format!("+{added}")),
                        )
                        .child(
                            div()
                                .text_color(colors.diff_removed_text)
                                .child(format!("−{removed}")),
                        )
                    })
                    .children(self.hint.clone().map(|hint| {
                        div()
                            .id("diff-hint")
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .cursor_pointer()
                            .text_color(colors.warning)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.hint = None;
                                cx.notify();
                            }))
                            .child(hint)
                    })),
            )
            .child(
                toggle_button("diff-soft-wrap", "Soft wrap", self.options.soft_wrap, cx)
                    .text_xs()
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSoftWrap), cx)),
            )
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let toolbar = self.render_toolbar(cx).into_any_element();
        let root = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(toolbar);

        let list = match &self.status {
            ListStatus::Loading => {
                return root.child(placeholder("Listing changed files…", cx));
            }
            ListStatus::Failed(message) => {
                return root.child(
                    div()
                        .m_3()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.error.opacity(0.6))
                        .bg(colors.error.opacity(0.08))
                        .text_sm()
                        .text_color(colors.error)
                        .child(format!("Could not compute the diff: {message}")),
                );
            }
            ListStatus::Ready(list) if list.is_empty() => {
                return root.child(placeholder(
                    "No changes: head has the same tree as the merge-base.",
                    cx,
                ));
            }
            ListStatus::Ready(list) => list.clone(),
        };

        self.update_metrics(window, cx);
        self.update_wrap();
        let mut viewport = self.list_bounds().size.height;
        if viewport <= Pixels::ZERO {
            viewport = window.viewport_size().height;
        }
        self.prefetch(viewport, cx);

        let row_height = self.metrics.row_height;
        let total = self.total_rows();
        let mut rows = uniform_list(
            "diff-rows",
            total,
            cx.processor(|this, range: Range<usize>, window, cx| {
                this.render_rows(range, window, cx)
            }),
        )
        .track_scroll(&self.scroll)
        .size_full();
        // Horizontal gestures scroll the code, not the rows.
        rows.interactivity().base_style.restrict_scroll_to_axis = Some(true);

        let top_file = self.file_at(self.top_row());
        let sticky = self.render_file_header(top_file, &list.entries()[top_file], true, cx);
        // The next file's header pushes the sticky one up as it arrives.
        let next_header = self.starts[top_file + 1] as f64 * self.row_height() - self.scroll_top();
        let sticky_top = px((next_header - self.row_height()).min(0.0) as f32);
        let scrollbar = self.scrollbar().map(|geometry| {
            let dragging = matches!(self.drag, Some(Drag::Scrollbar { .. }));
            div()
                .id("diff-scrollbar")
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(SCROLLBAR_WIDTH)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, _, cx| {
                        this.scrollbar_down(event, cx)
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(geometry.thumb_top))
                        .left(px(3.0))
                        .right(px(3.0))
                        .h(px(geometry.thumb_height))
                        .rounded_full()
                        .bg(colors.text_muted.opacity(if dragging { 0.55 } else { 0.3 }))
                        .hover(|style| style.bg(colors.text_muted.opacity(0.5))),
                )
        });
        // While dragging, follow the mouse anywhere in the window.
        let drag_listener = self.drag.is_some().then(|| {
            let view = cx.entity();
            canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let moved = view.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble {
                            moved.update(cx, |this, cx| this.drag_move(event, cx));
                        }
                    });
                    let released = view;
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                            released.update(cx, |this, cx| this.drag_end(cx));
                        }
                    });
                },
            )
            .absolute()
            .size_full()
        });

        root.child(
            div()
                .id("diff-view")
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .font_family(theme.fonts.code_family.clone())
                .text_size(theme.fonts.code_size)
                .line_height(row_height)
                .text_color(colors.text)
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    this.scroll_horizontally(event, cx)
                }))
                // A click can be released before the drag listener below is
                // installed by the next frame.
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| this.drag_end(cx)),
                )
                .child(rows)
                .child(
                    div()
                        .absolute()
                        .top(sticky_top)
                        .left_0()
                        .right(SCROLLBAR_WIDTH)
                        .h(row_height)
                        .shadow_sm()
                        .block_mouse_except_scroll()
                        .child(sticky),
                )
                .children(scrollbar)
                .children(drag_listener),
        )
    }
}

/// Text runs of a display line: word-diff backgrounds and subtle whitespace
/// marks, as the sorted, non-overlapping ranges [`StyledText`] wants.
fn highlight_runs(
    display: &DisplayLine,
    words: &[Range<usize>],
    word_background: Hsla,
    mark_color: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let word_ranges: Vec<Range<usize>> = words
        .iter()
        .filter_map(|range| display.map_range(range.clone()))
        .collect();
    let marks: Vec<Range<usize>> = display
        .trailing_whitespace
        .iter()
        .chain(display.crlf.iter())
        .filter(|range| !range.is_empty())
        .cloned()
        .collect();
    if word_ranges.is_empty() && marks.is_empty() {
        return Vec::new();
    }
    let mut bounds: Vec<usize> = word_ranges
        .iter()
        .chain(&marks)
        .flat_map(|range| [range.start, range.end])
        .collect();
    bounds.sort_unstable();
    bounds.dedup();
    let inside = |ranges: &[Range<usize>], at: usize| ranges.iter().any(|r| r.contains(&at));
    bounds
        .windows(2)
        .filter_map(|pair| {
            let (start, end) = (pair[0], pair[1]);
            let word = inside(&word_ranges, start);
            let mark = inside(&marks, start);
            (word || mark).then(|| {
                (
                    start..end,
                    HighlightStyle {
                        background_color: word.then_some(word_background),
                        color: mark.then_some(mark_color),
                        ..HighlightStyle::default()
                    },
                )
            })
        })
        .collect()
}

/// Why a text file shows no hunks.
fn no_hunks_message(entry: &FileEntry) -> String {
    if let Some((old, new)) = entry.mode_change() {
        return format!("File mode changed from {} to {}.", old.mode(), new.mode());
    }
    match entry.kind {
        ChangeKind::Renamed => "File renamed without changes.".to_owned(),
        ChangeKind::Added => "Empty file added.".to_owned(),
        ChangeKind::Deleted => "Empty file deleted.".to_owned(),
        _ => "No content changes.".to_owned(),
    }
}

/// `1.2 MB`-style size.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
