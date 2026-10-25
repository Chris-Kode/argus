//! The run panel (spec §3.1, §6.3): the runs of the open repository, each
//! with its live log.
//!
//! - **List.** Active runs first, then recent ones, newest first. Finished
//!   runs (accepted, dismissed, rejected, stopped, empty, or followed by a
//!   newer run of their comment) are folded away behind "Show finished", so
//!   the list holds what is running or waiting for the reviewer
//!   ([`finished_runs`]). Each run is a compact, neutral card: the comment's
//!   first line, its state and `file:line`, the elapsed time and Stop while
//!   the run is live, and its latest step while it runs; only the status
//!   icon is colored. Model, effort, turns, cost and duration belong to the
//!   log. Clicking a card opens its log in a dialog and asks the workspace
//!   to scroll the diff to its comment ([`RunPanelEvent::OpenComment`]); a
//!   thread's "What Claude did" opens a run's log directly
//!   ([`RunPanel::show_run`]).
//! - **Log.** The selected run's events are folded into rows by
//!   [`RunLog`]: a stored log is rebuilt on the tokio runtime, then live
//!   events are appended as they stream. The log is a virtualized [`list`],
//!   so only rows on screen are laid out, and it follows the tail until the
//!   user scrolls up (and again once they scroll back to the bottom).
//! - **Actions.** View the prompt, reveal the worktree in the file manager,
//!   or open a terminal in it.
//!
//! Every storage read, log rebuild, git read and process spawn runs on the
//! tokio runtime; this view only receives results.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argus_git::Sha;
use argus_runs::log::tool_label;
use argus_runs::stream::{AssistantContent, StreamEvent, UserContent};
use argus_runs::{LogEntry, RunLog, RunStreamEvent, RunUpdate};
use argus_store::{CommentId, CommentStatus, RepoId, RunId, RunListing, RunStatus};
use gpui::{
    AnyElement, App, Context, EventEmitter, FollowMode, FontWeight, Hsla, ListAlignment, ListState,
    MouseButton, SharedString, Task, Window, div, list, prelude::*, px, relative,
};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::AppContext;
use crate::components::{
    button, claude_activity, disabled_button, disabled_icon_button, icon_button, kbd, placeholder,
    radius, toggle_button,
};
use crate::icons::{IconName, icon};
use crate::jobs::BackgroundJobs;
use crate::markdown::{self, MarkdownCache};
use crate::runtime::spawn_job;
use crate::theme::{ActiveTheme, Theme, ThemeColors};

/// Recent runs listed per repository.
const RECENT_RUNS: usize = 200;
/// Pixels of cards rendered beyond the list's viewport.
const CARD_OVERDRAW: f32 = 200.0;
/// Space around the card list and between cards.
const CARD_GAP: f32 = 4.0;
const LIST_PADDING: f32 = 6.0;
/// Width of a card's status icon column; the lines under it are indented
/// by it plus the gap, so they line up with the summary.
const STATUS_ICON: f32 = 14.0;
const CARD_INDENT: f32 = STATUS_ICON + 8.0;
/// Latest steps a running card shows.
const LIVE_STEPS: usize = 1;
/// Text sizes of a card.
const TITLE_TEXT: f32 = 12.5;
const MONO_TEXT: f32 = 11.0;
/// Largest size of the log dialog; it shrinks with the window.
const DIALOG_WIDTH: f32 = 880.0;
const DIALOG_HEIGHT: f32 = 720.0;
/// Live events applied per repaint at most; the rest follow at once.
const EVENT_BATCH: usize = 512;
/// Pixels rendered beyond the log's viewport, so short scrolls do not
/// show empty space.
const LOG_OVERDRAW: f32 = 400.0;
/// How often elapsed times of live runs repaint.
const TICK: Duration = Duration::from_secs(1);
/// Longest tool input or output shown inline, in lines, when expanded.
const MAX_DETAIL_LINES: usize = 400;

/// What the panel reports to the workspace.
#[derive(Clone, Debug)]
pub enum RunPanelEvent {
    /// Scroll the diff to this comment.
    OpenComment(CommentId),
    /// A run started; the panel should be shown.
    Started(RunId),
    /// Open the proposal of a run that changed something.
    ViewProposal(RunId),
    /// A message for the status bar.
    Notice {
        /// The text.
        message: SharedString,
        /// Whether it reports a failure.
        is_error: bool,
    },
}

/// One tool call of a live run, for its card.
#[derive(Clone, Debug)]
struct Step {
    id: String,
    label: String,
    failed: bool,
}

/// The latest steps of a live run, folded from its stream. Only the last
/// [`LIVE_STEPS`] are kept; the full log is the detail view's.
#[derive(Clone, Debug, Default)]
struct LiveSteps {
    cwd: Option<String>,
    steps: VecDeque<Step>,
}

impl LiveSteps {
    /// Applies one event; returns whether the steps changed.
    fn push(&mut self, event: &StreamEvent) -> bool {
        let mut changed = false;
        match event {
            StreamEvent::System(init) => self.cwd = init.cwd.clone(),
            StreamEvent::Assistant(message) => {
                for block in &message.content {
                    if let AssistantContent::ToolUse { id, name, input } = block {
                        self.steps.push_back(Step {
                            id: id.clone(),
                            label: tool_label(name, input, self.cwd.as_deref()),
                            failed: false,
                        });
                        if self.steps.len() > LIVE_STEPS {
                            self.steps.pop_front();
                        }
                        changed = true;
                    }
                }
            }
            StreamEvent::User(message) => {
                for block in &message.content {
                    if let UserContent::ToolResult(result) = block
                        && let Some(step) = self
                            .steps
                            .iter_mut()
                            .find(|step| step.id == result.tool_use_id)
                    {
                        step.failed = result.is_error;
                        changed = true;
                    }
                }
            }
            StreamEvent::Result(_) | StreamEvent::Unknown(_) => {}
        }
        changed
    }
}

/// The selected run's log.
struct LogView {
    run_id: RunId,
    log: RunLog,
    state: ListState,
    /// The stored events are still loading; live ones wait in `pending`.
    loading: bool,
    pending: Vec<(i64, Arc<StreamEvent>)>,
    /// Expanded rows (tool calls, thinking, unknown events).
    expanded: HashSet<usize>,
    error: Option<SharedString>,
    _load: Option<Task<()>>,
}

/// The run panel.
pub struct RunPanel {
    repo_id: Option<RepoId>,
    runs: Vec<RunListing>,
    /// Runs that are over and done with ([`finished_runs`]).
    finished: HashSet<RunId>,
    /// Indices in `runs` of the cards listed.
    shown: Vec<usize>,
    /// Finished runs are listed too.
    show_finished: bool,
    stopping: HashSet<RunId>,
    /// Latest steps of the live runs.
    steps: HashMap<RunId, LiveSteps>,
    /// Lines added and removed by runs ready to review; `None` when the
    /// worktree could not be read.
    line_counts: HashMap<RunId, Option<(u64, u64)>>,
    /// Comments whose run is being started.
    starting: HashSet<CommentId>,
    /// The run whose log dialog is open.
    selected: Option<RunId>,
    log: Option<LogView>,
    show_prompt: bool,
    /// The card list; cards differ in height, so it is a measured [`list`].
    cards: ListState,
    markdown: MarkdownCache,
    list_task: Option<Task<()>>,
    counts_task: Option<Task<()>>,
    open_task: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<RunPanelEvent> for RunPanel {}

fn markdown_cache(panel: &mut RunPanel) -> &mut MarkdownCache {
    &mut panel.markdown
}

impl RunPanel {
    /// A panel following the run manager of the [`AppContext`] global.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let manager = cx.global::<AppContext>().runs.clone();
        let mut updates = manager.subscribe_runs();
        let updates_task = cx.spawn(async move |this, cx| {
            loop {
                let update = match updates.recv().await {
                    Ok(update) => Some(update),
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => break,
                };
                let alive = this
                    .update(cx, |this, cx| this.on_update(update, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        let mut events = manager.subscribe_events();
        let events_task = cx.spawn(async move |this, cx| {
            loop {
                let mut lagged = false;
                let mut batch = Vec::new();
                match events.recv().await {
                    Ok(event) => batch.push(event),
                    Err(RecvError::Lagged(_)) => lagged = true,
                    Err(RecvError::Closed) => break,
                }
                while batch.len() < EVENT_BATCH {
                    match events.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(TryRecvError::Lagged(_)) => lagged = true,
                        Err(_) => break,
                    }
                }
                let alive = this
                    .update(cx, |this, cx| this.on_events(batch, lagged, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        let tick_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if this.runs.iter().any(|run| run.run.status.is_active()) {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        Self {
            repo_id: None,
            runs: Vec::new(),
            finished: HashSet::new(),
            shown: Vec::new(),
            show_finished: false,
            stopping: HashSet::new(),
            steps: HashMap::new(),
            line_counts: HashMap::new(),
            starting: HashSet::new(),
            selected: None,
            log: None,
            show_prompt: false,
            cards: ListState::new(0, ListAlignment::Top, px(CARD_OVERDRAW)),
            markdown: MarkdownCache::default(),
            list_task: None,
            counts_task: None,
            open_task: None,
            _tasks: vec![updates_task, events_task, tick_task],
        }
    }

    // ---- state ----------------------------------------------------------------

    /// Lists the runs of this repository (`None` clears the panel).
    pub fn set_repo(&mut self, repo_id: Option<RepoId>, cx: &mut Context<Self>) {
        if self.repo_id == repo_id {
            return;
        }
        self.repo_id = repo_id;
        self.runs.clear();
        self.finished.clear();
        self.shown.clear();
        self.cards.reset(0);
        self.counts_task = None;
        self.selected = None;
        self.log = None;
        self.show_prompt = false;
        self.reload(cx);
        cx.notify();
    }

    /// Where a listed run works and the first line of its comment, e.g.
    /// `src/lib.rs:12 — Rename this`; `None` when the run is not listed.
    pub fn run_context(&self, run_id: RunId) -> Option<String> {
        let listing = self.runs.iter().find(|listing| listing.run.id == run_id)?;
        let snippet = listing
            .body_md
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .trim();
        let location = if listing.run.comment_id.is_some() {
            format!("{}:{}", listing.file_path, listing.start_line)
        } else {
            listing.file_path.clone()
        };
        Some(if snippet.is_empty() {
            location
        } else {
            format!("{location} — {snippet}")
        })
    }

    /// Whether a run of this comment is being started.
    pub fn is_starting(&self, comment_id: CommentId) -> bool {
        self.starting.contains(&comment_id)
    }

    /// Starts a run of `comment_id` with `model` and `effort`. The start
    /// itself runs on the tokio runtime and is never cut short; once the
    /// process runs, the panel reports [`RunPanelEvent::Started`]. Its log
    /// stays closed: the card shows its progress.
    pub fn start(
        &mut self,
        comment_id: CommentId,
        model: String,
        effort: String,
        cx: &mut Context<Self>,
    ) {
        if !self.starting.insert(comment_id) {
            return;
        }
        let app = cx.global::<AppContext>();
        let manager = app.runs.clone();
        let handle = app
            .runtime
            .spawn(async move { manager.start(comment_id, model, effort).await });
        cx.spawn(async move |this, cx| {
            let result = handle.await;
            this.update(cx, |this, cx| {
                this.starting.remove(&comment_id);
                match result {
                    Ok(Ok(run_id)) => cx.emit(RunPanelEvent::Started(run_id)),
                    Ok(Err(error)) => {
                        tracing::warn!(comment = %comment_id, %error, "cannot start the run");
                        cx.emit(RunPanelEvent::Notice {
                            message: format!("Could not start the run: {error}").into(),
                            is_error: true,
                        });
                    }
                    Err(error) => {
                        cx.emit(RunPanelEvent::Notice {
                            message: format!("Could not start the run: {error}").into(),
                            is_error: true,
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Asks a live run to stop; the run manager sends `SIGTERM`, then
    /// `SIGKILL` after its grace period. The row updates when it ends.
    fn stop(&mut self, run_id: RunId, cx: &mut Context<Self>) {
        if cx.global::<AppContext>().runs.stop(run_id) {
            self.stopping.insert(run_id);
        } else {
            cx.emit(RunPanelEvent::Notice {
                message: "That run is no longer running.".into(),
                is_error: false,
            });
            self.reload(cx);
        }
        cx.notify();
    }

    fn listing(&self, run_id: RunId) -> Option<&RunListing> {
        self.runs.iter().find(|listing| listing.run.id == run_id)
    }

    /// Whether `listing` holds a proposal waiting for review.
    fn is_reviewable(&self, listing: &RunListing) -> bool {
        listing.run.status == RunStatus::Succeeded && !self.finished.contains(&listing.run.id)
    }

    /// Reloads the list when `comment_id` (whose status may have changed,
    /// e.g. dismissed) has runs listed.
    pub fn comment_changed(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        if self
            .runs
            .iter()
            .any(|listing| listing.run.comment_id == Some(comment_id))
        {
            self.reload(cx);
        }
    }

    fn shown_ids(&self) -> Vec<RunId> {
        self.shown
            .iter()
            .map(|&index| self.runs[index].run.id)
            .collect()
    }

    /// Recomputes the finished runs and the listed cards; `before` is the
    /// cards listed until now.
    fn rebuild_shown(&mut self, before: Vec<RunId>) {
        let finished = finished_runs(&self.runs);
        self.finished = self
            .runs
            .iter()
            .zip(&finished)
            .filter(|(_, finished)| **finished)
            .map(|(listing, _)| listing.run.id)
            .collect();
        self.shown = (0..self.runs.len())
            .filter(|&index| self.show_finished || !finished[index])
            .collect();
        if self.shown_ids() == before {
            // Same cards, maybe new states: keep the reading position.
            self.cards.remeasure();
        } else {
            self.cards.splice(0..before.len(), self.shown.len());
        }
    }

    fn toggle_finished(&mut self, cx: &mut Context<Self>) {
        let before = self.shown_ids();
        self.show_finished = !self.show_finished;
        self.rebuild_shown(before);
        cx.notify();
    }

    /// Reloads the list from storage; replacing the task cancels a reload
    /// in flight.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(repo_id) = self.repo_id else {
            self.list_task = None;
            return;
        };
        let store = cx.global::<AppContext>().store.clone();
        let manager = cx.global::<AppContext>().runs.clone();
        let job = spawn_job(cx, async move {
            let mut runs = store
                .runs()
                .list_recent_for_repo(repo_id, RECENT_RUNS)
                .await?;
            // Pull-before-push resolutions have no comment to list them by.
            let mut resolutions = manager.resolution_runs(repo_id).await?;
            resolutions.append(&mut runs);
            resolutions.sort_by_key(|listing| std::cmp::Reverse(listing.run.created_at));
            Ok::<_, argus_store::StoreError>(resolutions)
        });
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.list_task = None;
                match result {
                    Ok(Ok(mut runs)) if this.repo_id == Some(repo_id) => {
                        // Active first; each group newest first (as loaded).
                        runs.sort_by_key(|listing| !listing.run.status.is_active());
                        this.stopping.retain(|id| {
                            runs.iter()
                                .any(|l| l.run.id == *id && l.run.status.is_active())
                        });
                        let before = this.shown_ids();
                        this.runs = runs;
                        this.rebuild_shown(before);
                        this.load_line_counts(cx);
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "cannot list runs");
                        cx.emit(RunPanelEvent::Notice {
                            message: format!("Could not list the runs: {error}").into(),
                            is_error: true,
                        });
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => tracing::warn!(%error, "run list task failed"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn on_update(&mut self, update: Option<RunUpdate>, cx: &mut Context<Self>) {
        let active = cx.global::<AppContext>().runs.active_runs().len();
        BackgroundJobs::update(cx, |jobs| jobs.active_runs = active);
        let live = cx.global::<AppContext>().runs.active_runs();
        self.steps.retain(|run_id, _| live.contains(run_id));
        match &update {
            Some(RunUpdate::Finished(finished)) => {
                self.stopping.remove(&finished.run_id);
                self.line_counts.remove(&finished.run_id);
            }
            Some(RunUpdate::Accepted { run_id, .. } | RunUpdate::Rejected { run_id, .. }) => {
                self.line_counts.remove(run_id);
            }
            Some(RunUpdate::Started { .. }) | None => {}
        }
        self.reload(cx);
    }

    fn on_events(&mut self, batch: Vec<RunStreamEvent>, lagged: bool, cx: &mut Context<Self>) {
        let mut stepped = HashSet::new();
        for event in &batch {
            if self
                .steps
                .entry(event.run_id)
                .or_default()
                .push(&event.event)
            {
                stepped.insert(event.run_id);
            }
        }
        // A card gained a step line: measure it again.
        for (index, _) in self
            .runs
            .iter()
            .enumerate()
            .filter(|(_, listing)| stepped.contains(&listing.run.id))
        {
            self.cards.remeasure_items(index..index + 1);
        }
        if let Some(view) = self.log.as_mut() {
            let run_id = view.run_id;
            for event in batch.iter().filter(|event| event.run_id == run_id) {
                if view.loading {
                    view.pending.push((event.seq, event.event.clone()));
                    continue;
                }
                apply_change(view, event.seq, &event.event);
            }
        }
        if lagged && let Some(run_id) = self.selected {
            // Events were missed: rebuild the log from storage.
            self.load_log(run_id, cx);
        }
        cx.notify();
    }

    /// Opens the log dialog of `run_id` (a thread's "What Claude did").
    pub fn show_run(&mut self, run_id: RunId, cx: &mut Context<Self>) {
        self.select(run_id, cx);
    }

    /// Opens the log dialog of the newest run of `comment_id` (a running
    /// thread's "Running…"); nothing when it has none listed yet.
    pub fn show_comment_run(&mut self, comment_id: CommentId, cx: &mut Context<Self>) {
        let newest = self
            .runs
            .iter()
            .filter(|listing| listing.run.comment_id == Some(comment_id))
            .max_by_key(|listing| listing.run.created_at)
            .map(|listing| listing.run.id);
        if let Some(run_id) = newest {
            self.select(run_id, cx);
        }
    }

    /// Opens the log dialog of a run and loads its log.
    fn select(&mut self, run_id: RunId, cx: &mut Context<Self>) {
        if self.selected != Some(run_id) {
            self.selected = Some(run_id);
            self.show_prompt = false;
            self.load_log(run_id, cx);
        }
        cx.notify();
    }

    /// Whether a run's log dialog is open.
    pub fn has_dialog(&self) -> bool {
        self.log.is_some()
    }

    /// Closes the log dialog.
    pub fn close_dialog(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.log = None;
        self.show_prompt = false;
        cx.notify();
    }

    /// Rebuilds the run's log from `run_events` on the tokio runtime. Live
    /// events arriving meanwhile are queued and applied after it, skipping
    /// the ones already stored.
    fn load_log(&mut self, run_id: RunId, cx: &mut Context<Self>) {
        let store = cx.global::<AppContext>().store.clone();
        let job = spawn_job(cx, async move {
            let events = store
                .runs()
                .events(run_id)
                .await
                .map_err(|error| error.to_string())?;
            // Thousands of lines to parse: keep them off the async workers.
            tokio::task::spawn_blocking(move || RunLog::from_stored(&events))
                .await
                .map_err(|error| error.to_string())
        });
        let task = cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                let Some(view) = this.log.as_mut().filter(|view| view.run_id == run_id) else {
                    return;
                };
                view._load = None;
                view.loading = false;
                match result {
                    Ok(Ok(log)) => {
                        view.log = log;
                        view.state.reset(view.log.len());
                        view.state.set_follow_mode(FollowMode::Tail);
                        for (seq, event) in std::mem::take(&mut view.pending) {
                            apply_change(view, seq, &event);
                        }
                    }
                    Ok(Err(error)) => view.error = Some(error.to_string().into()),
                    Err(error) => view.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        });
        let state = ListState::new(0, ListAlignment::Top, px(LOG_OVERDRAW));
        state.set_follow_mode(FollowMode::Tail);
        self.log = Some(LogView {
            run_id,
            log: RunLog::new(),
            state,
            loading: true,
            pending: Vec::new(),
            expanded: HashSet::new(),
            error: None,
            _load: Some(task),
        });
    }

    /// Counts the lines of the runs ready to review that are not counted
    /// yet, on the tokio runtime; replacing the task cancels a count in
    /// flight.
    fn load_line_counts(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<(RunId, PathBuf, Option<Sha>)> = self
            .runs
            .iter()
            .filter(|listing| self.is_reviewable(listing))
            .filter(|listing| !self.line_counts.contains_key(&listing.run.id))
            .map(|listing| {
                (
                    listing.run.id,
                    listing.run.worktree_path.clone(),
                    Sha::parse(&listing.run.start_sha),
                )
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        let worktrees = cx.global::<AppContext>().worktrees.clone();
        let job = spawn_job(cx, async move {
            let mut counts = Vec::with_capacity(wanted.len());
            for (run_id, path, start_sha) in wanted {
                let count = match start_sha {
                    Some(start_sha) => {
                        match worktrees.staged_line_counts(&path, &start_sha).await {
                            Ok(count) => Some(count),
                            Err(error) => {
                                tracing::debug!(run = %run_id, %error, "cannot count the run's lines");
                                None
                            }
                        }
                    }
                    None => None,
                };
                counts.push((run_id, count));
            }
            counts
        });
        self.counts_task = Some(cx.spawn(async move |this, cx| {
            let Ok(counts) = job.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.counts_task = None;
                this.line_counts.extend(counts);
                cx.notify();
            })
            .ok();
        }));
    }

    fn toggle_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(view) = self.log.as_mut() {
            if !view.expanded.remove(&index) {
                view.expanded.insert(index);
            }
            // Re-measure the row; the list keeps the reading position.
            view.state.splice(index..index + 1, 1);
            cx.notify();
        }
    }

    /// Reveals the worktree in the file manager, or opens a terminal in it.
    fn open_worktree(&mut self, path: PathBuf, terminal: bool, cx: &mut Context<Self>) {
        let job = spawn_job(cx, {
            let path = path.clone();
            async move {
                match tokio::fs::metadata(&path).await {
                    Ok(metadata) if metadata.is_dir() => {}
                    _ => return Err("The run's worktree no longer exists.".to_owned()),
                }
                if terminal {
                    open_terminal(&path).await
                } else {
                    Ok(())
                }
            }
        });
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |_, cx| match result {
                Ok(Ok(())) => {
                    if !terminal {
                        cx.reveal_path(&path);
                    }
                }
                Ok(Err(message)) => cx.emit(RunPanelEvent::Notice {
                    message: message.into(),
                    is_error: true,
                }),
                Err(error) => cx.emit(RunPanelEvent::Notice {
                    message: error.to_string().into(),
                    is_error: true,
                }),
            })
            .ok();
        }));
    }

    // ---- rendering ------------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .h(px(32.0))
            .px_3()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                div()
                    .text_size(px(TITLE_TEXT))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child("Runs"),
            )
            .child(div().flex_1())
            .when(!self.finished.is_empty(), |header| {
                header.child(
                    button(
                        "runs-show-finished",
                        if self.show_finished {
                            "Hide finished".to_owned()
                        } else {
                            format!("Show finished ({})", self.finished.len())
                        },
                        cx,
                    )
                    .text_xs()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_finished(cx))),
                )
            })
            .into_any_element()
    }

    /// One run card, with its share of the list's padding and gaps.
    fn render_card(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(listing) = self.shown.get(index).and_then(|&run| self.runs.get(run)) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let mono = theme.fonts.code_family.clone();
        let run = &listing.run;
        let id = run.id;
        let comment_id = run.comment_id;
        let status = run.status;
        let selected = self.selected == Some(id);
        let finished = self.finished.contains(&id);
        let style = card_style(status, finished.then_some(listing.comment_status), colors);

        let snippet = markdown::first_line_plain(&listing.body_md);
        let location = if comment_id.is_some() {
            format!("{}:{}", listing.file_path, listing.start_line)
        } else {
            listing.file_path.clone()
        };
        // A live clock while Claude works; how long a finished run took is
        // in its log.
        let elapsed = matches!(status, RunStatus::Running | RunStatus::Queued)
            .then(|| format_elapsed(elapsed_ms(run, now_ms())));
        let line_counts = (status == RunStatus::Succeeded && !finished)
            .then(|| self.line_counts.get(&id).copied().flatten())
            .flatten();

        let stop = status.is_active().then(|| {
            if self.stopping.contains(&id) {
                disabled_icon_button(("run-stop", index), IconName::X, "Stopping…", cx)
                    .into_any_element()
            } else {
                let tooltip = if status == RunStatus::Queued {
                    "Cancel"
                } else {
                    "Stop"
                };
                icon_button(("run-stop", index), IconName::X, tooltip, cx)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.stop(id, cx);
                    }))
                    .into_any_element()
            }
        });
        let top = div()
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .h(px(22.0))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .justify_center()
                    .w(px(STATUS_ICON))
                    .child(status_icon(status, &style, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(TITLE_TEXT))
                    .text_color(if style.quiet {
                        colors.text_muted
                    } else {
                        colors.text
                    })
                    .child(snippet),
            )
            .children(elapsed.map(|elapsed| {
                div()
                    .flex_none()
                    .font_family(mono.clone())
                    .text_size(px(MONO_TEXT))
                    .text_color(colors.text_placeholder)
                    .child(elapsed)
            }))
            .children(stop);

        let meta = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .min_w_0()
            .pl(px(CARD_INDENT))
            .text_size(px(MONO_TEXT))
            .text_color(colors.text_placeholder)
            .child(div().flex_none().child(style.text))
            .child(div().flex_none().child("·"))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_family(mono.clone())
                    .child(location),
            )
            .when_some(line_counts, |row, (added, removed)| {
                row.child(
                    div()
                        .flex_none()
                        .font_family(mono.clone())
                        .text_color(colors.diff_added_text)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(mono.clone())
                        .text_color(colors.diff_removed_text)
                        .child(format!("−{removed}")),
                )
            });

        let step = (status == RunStatus::Running)
            .then(|| {
                self.steps
                    .get(&id)
                    .and_then(|live| live.steps.back().cloned())
            })
            .flatten()
            .map(|step| {
                let color = if step.failed {
                    colors.error
                } else {
                    colors.text_muted
                };
                div()
                    .min_w_0()
                    .pl(px(CARD_INDENT))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_family(mono.clone())
                    .text_size(px(MONO_TEXT))
                    .text_color(color)
                    .child(step.label)
            });

        let card = div()
            .id(("run-card", index))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .pl(px(8.0))
            .pr(px(4.0))
            .py(px(4.0))
            .min_w_0()
            .rounded(radius::md(cx))
            .border_1()
            .border_color(colors.border_variant)
            .cursor_pointer()
            .when(selected, |card| card.bg(colors.element_selected))
            .hover(|style| style.bg(colors.element_hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select(id, cx);
                if let Some(comment_id) = comment_id {
                    cx.emit(RunPanelEvent::OpenComment(comment_id));
                }
            }))
            .child(top)
            .child(meta)
            .children(step);
        let last = index + 1 == self.shown.len();
        div()
            .px(px(LIST_PADDING))
            .when(index == 0, |item| item.pt(px(LIST_PADDING)))
            .pb(px(if last { LIST_PADDING } else { CARD_GAP }))
            .child(card)
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.shown.is_empty() {
            let text = if self.repo_id.is_none() {
                "No repository open."
            } else if self.list_task.is_some() && self.runs.is_empty() {
                "Loading runs…"
            } else if self.runs.is_empty() {
                "No runs yet."
            } else {
                "Nothing running or waiting for review."
            };
            return placeholder(text, cx).into_any_element();
        }
        let this = cx.entity();
        list(self.cards.clone(), move |index, _, cx| {
            this.update(cx, |this, cx| this.render_card(index, cx))
        })
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    /// The open run's log dialog, drawn by the workspace over the window;
    /// `None` when no run is open.
    pub fn render_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let detail = self.log.as_ref().map(|view| self.render_detail(view, cx))?;
        let colors = cx.theme().colors.clone();
        Some(
            div()
                .id("run-dialog-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(colors.overlay)
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.close_dialog(cx)),
                )
                .child(
                    div()
                        .id("run-dialog")
                        .flex()
                        .flex_col()
                        .w(relative(0.85))
                        .max_w(px(DIALOG_WIDTH))
                        .h(relative(0.85))
                        .max_h(px(DIALOG_HEIGHT))
                        .rounded(radius::md(cx))
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.elevated_surface)
                        .shadow_lg()
                        .overflow_hidden()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(detail),
                )
                .into_any_element(),
        )
    }

    fn render_detail(&self, view: &LogView, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let mono = cx.theme().fonts.code_family.clone();
        let listing = self.listing(view.run_id);
        let title = listing
            .map(|listing| markdown::first_line_plain(&listing.body_md))
            .unwrap_or_else(|| "Run".into());
        let status = listing.map(|listing| {
            let finished = self
                .finished
                .contains(&listing.run.id)
                .then_some(listing.comment_status);
            let style = card_style(listing.run.status, finished, &colors);
            (status_icon(listing.run.status, &style, cx), style.text)
        });
        // The log is where the run's details live: model and effort here,
        // turns, cost and duration in its summary.
        let meta = listing.map(|listing| {
            let run = &listing.run;
            let mut meta = if run.comment_id.is_some() {
                format!("{}:{}", listing.file_path, listing.start_line)
            } else {
                listing.file_path.clone()
            };
            meta.push_str(" · ");
            meta.push_str(&run.model);
            if !run.effort.is_empty() {
                meta.push_str(" · ");
                meta.push_str(&run.effort);
            }
            meta
        });
        // A rejected run's worktree is gone.
        let worktree = listing
            .filter(|listing| listing.run.status != RunStatus::Rejected)
            .map(|listing| listing.run.worktree_path.clone());
        let proposal = listing
            .filter(|listing| self.is_reviewable(listing))
            .map(|listing| listing.run.id);
        let stop = listing
            .filter(|listing| listing.run.status.is_active())
            .map(|listing| {
                let id = listing.run.id;
                if self.stopping.contains(&id) {
                    disabled_button("run-dialog-stop", "Stopping…", cx)
                        .h(px(22.0))
                        .text_xs()
                        .into_any_element()
                } else {
                    let label = if listing.run.status == RunStatus::Queued {
                        "Cancel"
                    } else {
                        "Stop"
                    };
                    button("run-dialog-stop", label, cx)
                        .h(px(22.0))
                        .text_xs()
                        .on_click(cx.listener(move |this, _, _, cx| this.stop(id, cx)))
                        .into_any_element()
                }
            });

        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .h(px(40.0))
            .px_4()
            .border_b_1()
            .border_color(colors.border_variant)
            .children(status.map(|(icon, label)| {
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .child(icon)
                    .child(div().text_sm().text_color(colors.text_muted).child(label))
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child(title),
            )
            .child(
                kbd("Esc", cx)
                    .id("run-dialog-close")
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
            );
        let toolbar =
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(32.0))
                .pl_4()
                .pr_2()
                .border_b_1()
                .border_color(colors.border_variant)
                .text_xs()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .font_family(mono.clone())
                        .text_color(colors.text_placeholder)
                        .children(meta),
                )
                .children(stop)
                .children(proposal.map(|run_id| {
                    button("run-view-proposal", "View proposal", cx)
                        .h(px(22.0))
                        .text_xs()
                        .text_color(colors.accent)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_dialog(cx);
                            cx.emit(RunPanelEvent::ViewProposal(run_id));
                        }))
                }))
                .child(
                    toggle_button("run-view-prompt", "View prompt", self.show_prompt, cx)
                        .h(px(22.0))
                        .text_xs()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_prompt = !this.show_prompt;
                            cx.notify();
                        })),
                )
                .children(worktree.clone().map(|path| {
                    button("run-reveal", reveal_label(), cx)
                        .h(px(22.0))
                        .text_xs()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_worktree(path.clone(), false, cx)
                        }))
                }))
                .children(worktree.map(|path| {
                    button("run-terminal", "Terminal", cx)
                        .h(px(22.0))
                        .text_xs()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_worktree(path.clone(), true, cx)
                        }))
                }));

        let run_error = listing
            .and_then(|listing| listing.run.error.clone())
            .filter(|_| !self.show_prompt)
            .map(|error| {
                div()
                    .id("run-error")
                    .flex_none()
                    .max_h(px(120.0))
                    .overflow_y_scroll()
                    .px_3()
                    .py_1()
                    .bg(colors.error.opacity(0.08))
                    .border_b_1()
                    .border_color(colors.error.opacity(0.4))
                    .text_xs()
                    .text_color(colors.error)
                    .font_family(cx.theme().fonts.code_family.clone())
                    .child(error)
            });

        let body: AnyElement = if self.show_prompt {
            let prompt = listing
                .map(|listing| listing.run.prompt.clone())
                .unwrap_or_else(|| "Loading…".to_owned());
            div()
                .id("run-prompt")
                .size_full()
                .overflow_y_scroll()
                .p_3()
                .text_xs()
                .text_color(colors.text)
                .child(prompt)
                .into_any_element()
        } else if let Some(error) = &view.error {
            placeholder(format!("Could not load the log: {error}"), cx).into_any_element()
        } else if view.loading {
            placeholder("Loading the log…", cx).into_any_element()
        } else if view.log.is_empty() {
            let active = listing.is_some_and(|listing| listing.run.status.is_active());
            placeholder(
                if active {
                    "Waiting for Claude…"
                } else {
                    "This run logged no events."
                },
                cx,
            )
            .into_any_element()
        } else {
            let this = cx.entity();
            list(view.state.clone(), move |index, _, cx| {
                this.update(cx, |this, cx| this.render_entry(index, cx))
            })
            .size_full()
            .py_1()
            .into_any_element()
        };
        // The log and the prompt fill the dialog under the toolbar, on the
        // editor background, in the code font.
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(colors.background)
            .font_family(cx.theme().fonts.code_family.clone())
            .child(body);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(header)
            .child(toolbar)
            .children(run_error)
            .child(body)
            .into_any_element()
    }

    /// One row of the log.
    fn render_entry(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.log.as_ref() else {
            return div().into_any_element();
        };
        let Some(entry) = view.log.entries().get(index).cloned() else {
            return div().into_any_element();
        };
        let expanded = view.expanded.contains(&index);
        let pending_tool = self
            .listing(view.run_id)
            .is_some_and(|listing| listing.run.status.is_active());
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let code_font = theme.fonts.code_family.clone();
        let row = div().px_3().py_1().min_w_0();
        match entry {
            LogEntry::Started { model, version } => {
                let mut parts = vec!["Session started".to_owned()];
                if let Some(model) = model {
                    parts.push(model);
                }
                if let Some(version) = version {
                    parts.push(format!("Claude Code {version}"));
                }
                row.text_xs()
                    .text_color(colors.text_placeholder)
                    .child(parts.join(" · "))
                    .into_any_element()
            }
            LogEntry::Text(text) => {
                let doc = self.markdown.prepare(&text, markdown_cache, cx);
                row.text_sm()
                    .child(markdown::render(
                        &doc,
                        format!("run-log-{index}").into(),
                        cx,
                    ))
                    .into_any_element()
            }
            LogEntry::Thinking(text) => {
                let first = text.lines().next().unwrap_or_default().to_owned();
                expandable(
                    index,
                    "Thinking",
                    if expanded { String::new() } else { first },
                    colors.text_muted,
                    expanded,
                    cx,
                )
                .when(expanded, |row| row.child(detail_block(&text, &theme)))
                .into_any_element()
            }
            LogEntry::Tool(call) => {
                let color = if call.is_error {
                    colors.error
                } else {
                    colors.text
                };
                let status = match (&call.output, call.is_error) {
                    (_, true) => " · failed",
                    (None, false) if pending_tool => " · …",
                    _ => "",
                };
                expandable(index, &call.label, status.to_owned(), color, expanded, cx)
                    .font_family(code_font.clone())
                    .when(expanded, |row| {
                        row.child(detail_label("Input", colors))
                            .child(detail_block(&call.input, &theme))
                            .when_some(call.output.clone(), |row, output| {
                                row.child(detail_label("Output", colors)).child(
                                    detail_block(&output, &theme)
                                        .when(call.is_error, |b| b.text_color(colors.error)),
                                )
                            })
                    })
                    .into_any_element()
            }
            LogEntry::UserText(text) => row
                .text_sm()
                .text_color(colors.text_muted)
                .child(text)
                .into_any_element(),
            LogEntry::Error(text) => row
                .text_sm()
                .text_color(colors.error)
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(radius::sm(cx))
                        .border_1()
                        .border_color(colors.error.opacity(0.5))
                        .bg(colors.error.opacity(0.08))
                        .child(text),
                )
                .into_any_element(),
            LogEntry::Summary {
                text,
                turns,
                cost_usd,
                duration_ms,
            } => {
                let mut meta = Vec::new();
                if let Some(turns) = turns {
                    meta.push(format!("{turns} turn{}", if turns == 1 { "" } else { "s" }));
                }
                if let Some(cost) = cost_usd {
                    meta.push(format_cost(cost));
                }
                if let Some(duration) = duration_ms {
                    meta.push(format_elapsed(duration));
                }
                let doc = text
                    .as_deref()
                    .map(|text| self.markdown.prepare(text, markdown_cache, cx));
                let colors = &cx.theme().colors;
                // The run's last word: a log entry under a hairline.
                row.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .mt_1()
                        .pt_2()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .gap_2()
                                .text_xs()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(colors.text)
                                        .child("Summary"),
                                )
                                .child(div().text_color(colors.text_muted).child(meta.join(" · "))),
                        )
                        .children(doc.map(|doc| {
                            div().text_sm().child(markdown::render(
                                &doc,
                                format!("run-summary-{index}").into(),
                                cx,
                            ))
                        })),
                )
                .into_any_element()
            }
            LogEntry::Other { kind, json } => expandable(
                index,
                &kind,
                String::new(),
                colors.text_placeholder,
                expanded,
                cx,
            )
            .when(expanded, |row| row.child(detail_block(&json, &theme)))
            .into_any_element(),
        }
    }
}

impl Render for RunPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let list = self.render_list(cx);
        div()
            .id("run-panel")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(header)
            .child(list)
    }
}

/// Applies one live event to the shown log, re-measuring the rows it
/// touched.
fn apply_change(view: &mut LogView, seq: i64, event: &StreamEvent) {
    let change = view.log.push(seq, event);
    if change.is_empty() {
        return;
    }
    for row in change.updated {
        view.state.splice(row..row + 1, 1);
    }
    let appended = change.appended;
    if !appended.is_empty() {
        view.state
            .splice(appended.start..appended.start, appended.len());
    }
}

/// A clickable row header with a disclosure triangle.
fn expandable(
    index: usize,
    label: &str,
    trailing: String,
    color: Hsla,
    expanded: bool,
    cx: &mut Context<RunPanel>,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors.clone();
    div()
        .id(("run-log-entry", index))
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_0p5()
        .min_w_0()
        .child(
            div()
                .id(("run-log-toggle", index))
                .flex()
                .items_center()
                .gap_1()
                .min_w_0()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .text_xs()
                .hover(|style| style.bg(colors.element_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_entry(index, cx)))
                .child(icon(
                    if expanded {
                        IconName::CaretDown
                    } else {
                        IconName::CaretRight
                    },
                    px(12.0),
                    colors.text_muted,
                ))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_color(color)
                        .child(label.to_owned()),
                )
                .when(!trailing.is_empty(), |row| {
                    row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(colors.text_placeholder)
                            .child(trailing),
                    )
                }),
        )
}

fn detail_label(label: &'static str, colors: &ThemeColors) -> gpui::Div {
    div()
        .pl_4()
        .text_xs()
        .text_color(colors.text_placeholder)
        .child(label)
}

/// A tool payload in the code font, cut to [`MAX_DETAIL_LINES`].
fn detail_block(text: &str, theme: &Theme) -> gpui::Div {
    let colors = &theme.colors;
    let mut lines = text.lines();
    let mut shown: String = lines
        .by_ref()
        .take(MAX_DETAIL_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let rest = lines.count();
    if rest > 0 {
        shown.push_str(&format!("\n… {rest} more lines"));
    }
    if shown.is_empty() {
        shown = "(empty)".to_owned();
    }
    div()
        .ml_4()
        .px_2()
        .py_1()
        .rounded(theme.radius.sm)
        .bg(colors.surface)
        .font_family(theme.fonts.code_family.clone())
        .text_xs()
        .text_color(colors.text)
        .child(shown)
}

/// How a run card looks in each state: neutral, with the state in its
/// label and the color of its icon.
struct CardStyle {
    text: &'static str,
    /// The status icon.
    icon: Hsla,
    /// Over and done with: the summary is muted.
    quiet: bool,
}

/// The style of a card; `finished` is `Some(comment status)` for a run that
/// is over and done with.
fn card_style(
    status: RunStatus,
    finished: Option<Option<CommentStatus>>,
    colors: &ThemeColors,
) -> CardStyle {
    let quiet = |text| CardStyle {
        text,
        icon: colors.text_placeholder,
        quiet: true,
    };
    let live = |text, icon| CardStyle {
        text,
        icon,
        quiet: false,
    };
    match status {
        RunStatus::Succeeded if finished.is_some() => match finished.flatten() {
            Some(CommentStatus::Resolved) => quiet("Accepted"),
            Some(CommentStatus::Dismissed) => quiet("Dismissed"),
            _ => quiet("Followed up"),
        },
        RunStatus::Queued => live("Starting", colors.text_muted),
        RunStatus::Running => live("Running", colors.claude),
        RunStatus::Succeeded => live("Ready to review", colors.accent),
        RunStatus::Failed => live("Failed", colors.error),
        RunStatus::NoChanges => quiet("No changes"),
        RunStatus::Stopped => quiet("Stopped"),
        RunStatus::Rejected => quiet("Rejected"),
    }
}

/// The icon of a run's state, on cards and in the log dialog.
fn status_icon(status: RunStatus, style: &CardStyle, cx: &App) -> AnyElement {
    let (name, size) = match status {
        RunStatus::Running => return claude_activity(px(STATUS_ICON), cx),
        // Starting: the eye, still (motion is for live work).
        RunStatus::Queued => (IconName::ArgusEyeMono, STATUS_ICON),
        RunStatus::Succeeded => (IconName::GitPullRequest, 13.0),
        RunStatus::Failed => (IconName::Warning, 13.0),
        RunStatus::NoChanges => (IconName::Check, 12.0),
        RunStatus::Stopped | RunStatus::Rejected => (IconName::X, 12.0),
    };
    icon(name, px(size), style.icon).into_any_element()
}

/// Whether each of `runs` is over and done with: not active, and not the
/// latest run of its comment when that run is a proposal waiting for review
/// or a failure the comment is still open with.
fn finished_runs(runs: &[RunListing]) -> Vec<bool> {
    let mut latest: HashMap<CommentId, i64> = HashMap::new();
    for listing in runs {
        if let Some(comment_id) = listing.run.comment_id {
            let at = latest.entry(comment_id).or_insert(listing.run.created_at);
            *at = (*at).max(listing.run.created_at);
        }
    }
    runs.iter()
        .map(|listing| {
            let run = &listing.run;
            let is_latest = run
                .comment_id
                .is_none_or(|comment_id| latest.get(&comment_id) == Some(&run.created_at));
            let waiting = match run.status {
                RunStatus::Queued | RunStatus::Running => true,
                RunStatus::Succeeded => {
                    is_latest
                        && matches!(listing.comment_status, None | Some(CommentStatus::Proposed))
                }
                RunStatus::Failed => {
                    is_latest
                        && !matches!(
                            listing.comment_status,
                            Some(CommentStatus::Resolved | CommentStatus::Dismissed)
                        )
                }
                RunStatus::NoChanges | RunStatus::Stopped | RunStatus::Rejected => false,
            };
            !waiting
        })
        .collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// Wall time of a run: until now while it is live.
fn elapsed_ms(run: &argus_store::Run, now: i64) -> i64 {
    if run.status.is_active() {
        return now - run.created_at;
    }
    run.finished_at
        .map(|finished| finished - run.created_at)
        .or(run.duration_ms)
        .unwrap_or_default()
}

/// `12s`, `3m 04s`, `1h 02m`.
fn format_elapsed(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

fn format_cost(cost: f64) -> String {
    if cost < 0.01 && cost > 0.0 {
        format!("${cost:.3}")
    } else {
        format!("${cost:.2}")
    }
}

fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Finder"
    } else {
        "Files"
    }
}

/// Opens a terminal window in `path`: Terminal.app on macOS, the user's
/// `$TERMINAL` or the system's default terminal elsewhere.
async fn open_terminal(path: &std::path::Path) -> Result<(), String> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = tokio::process::Command::new("open");
        command.arg("-a").arg("Terminal").arg(path);
        command
    } else if cfg!(windows) {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/C", "start", "cmd"]);
        command
    } else {
        let terminal = std::env::var("TERMINAL")
            .ok()
            .filter(|terminal| !terminal.trim().is_empty())
            .unwrap_or_else(|| "x-terminal-emulator".to_owned());
        tokio::process::Command::new(terminal)
    };
    command
        .current_dir(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match command.spawn() {
        // The terminal outlives Argus' interest in it; reap it in the
        // background.
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            Ok(())
        }
        Err(error) => Err(format!("Could not open a terminal: {error}")),
    }
}
