//! The run panel (spec §3.1, §6.3): the runs of the open repository, each
//! with its live log.
//!
//! - **List.** Active runs first, then recent ones, newest first. Each run
//!   is a card styled by its state (Panoptes #56 §3, §5): a running run is
//!   gold with the pulsing eye, its elapsed time and its latest steps; a
//!   run ready to review is teal with its `+n −m`; failed runs carry an
//!   `error` label and the quiet states stay neutral. A card shows the
//!   comment's first line, `file:line`, `model · effort`, the turns and the
//!   cost once known, and Stop while the run is live. The header shows the
//!   number of runs listed. Clicking a card selects it and asks the
//!   workspace to scroll the diff to its comment
//!   ([`RunPanelEvent::OpenComment`]).
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
use argus_store::{CommentId, RepoId, RunId, RunListing, RunStatus};
use gpui::{
    AnyElement, Context, EventEmitter, FollowMode, FontWeight, Hsla, ListAlignment, ListState,
    SharedString, Task, Window, div, list, prelude::*, px,
};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::AppContext;
use crate::components::{
    button, claude_activity, disabled_button, icon_button, placeholder, radius, toggle_button,
};
use crate::icons::{IconName, icon};
use crate::jobs::BackgroundJobs;
use crate::markdown::{self, MarkdownCache};
use crate::runtime::spawn_job;
use crate::theme::{ActiveTheme, ThemeColors};

/// Recent runs listed per repository.
const RECENT_RUNS: usize = 200;
/// Height the card list takes when a log is shown under it.
const LIST_HEIGHT_WITH_LOG: f32 = 280.0;
/// Pixels of cards rendered beyond the list's viewport.
const CARD_OVERDRAW: f32 = 200.0;
/// Space around the card list and between cards (Panoptes #56 §5).
const CARD_GAP: f32 = 8.0;
const LIST_PADDING: f32 = 10.0;
/// Latest steps a running card shows.
const LIVE_STEPS: usize = 4;
/// Text sizes of a card (Panoptes #56 §5).
const TITLE_TEXT: f32 = 12.5;
const LABEL_TEXT: f32 = 12.0;
const MONO_TEXT: f32 = 11.0;
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

/// Live numbers of a run, from its stream.
#[derive(Clone, Copy, Debug, Default)]
struct LiveStats {
    /// Assistant messages so far.
    turns: u32,
    /// Claude's turn count from the result.
    final_turns: Option<u32>,
    /// Total cost from the result.
    cost_usd: Option<f64>,
}

/// One tool call of a live run, for its card.
#[derive(Clone, Debug)]
struct Step {
    id: String,
    label: String,
    finished: bool,
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
                            finished: false,
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
                        step.finished = true;
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
    live: HashMap<RunId, LiveStats>,
    stopping: HashSet<RunId>,
    /// Latest steps of the live runs.
    steps: HashMap<RunId, LiveSteps>,
    /// Lines added and removed by runs ready to review; `None` when the
    /// worktree could not be read.
    line_counts: HashMap<RunId, Option<(u64, u64)>>,
    /// Comments whose run is being started.
    starting: HashSet<CommentId>,
    selected: Option<RunId>,
    log: Option<LogView>,
    show_prompt: bool,
    /// Docked at the bottom: list and log side by side.
    horizontal: bool,
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
            live: HashMap::new(),
            stopping: HashSet::new(),
            steps: HashMap::new(),
            line_counts: HashMap::new(),
            starting: HashSet::new(),
            selected: None,
            log: None,
            show_prompt: false,
            horizontal: false,
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
        self.cards.reset(0);
        self.counts_task = None;
        self.selected = None;
        self.log = None;
        self.show_prompt = false;
        self.reload(cx);
        cx.notify();
    }

    /// Shows the list and the log side by side (bottom dock) or stacked.
    pub fn set_horizontal(&mut self, horizontal: bool, cx: &mut Context<Self>) {
        if self.horizontal != horizontal {
            self.horizontal = horizontal;
            cx.notify();
        }
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
    /// process runs, the panel selects the run and reports
    /// [`RunPanelEvent::Started`].
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
                    Ok(Ok(run_id)) => {
                        this.select(run_id, cx);
                        cx.emit(RunPanelEvent::Started(run_id));
                    }
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
                        let old = this.runs.len();
                        this.runs = runs;
                        if old == this.runs.len() {
                            // Same cards, maybe new states: keep the reading
                            // position.
                            this.cards.remeasure();
                        } else {
                            this.cards.splice(0..old, this.runs.len());
                        }
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
            let stats = self.live.entry(event.run_id).or_default();
            match event.event.as_ref() {
                StreamEvent::Assistant(_) => stats.turns += 1,
                StreamEvent::Result(result) => {
                    stats.final_turns = result.num_turns;
                    stats.cost_usd = result.total_cost_usd;
                }
                _ => {}
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

    /// Selects a run and loads its log.
    fn select(&mut self, run_id: RunId, cx: &mut Context<Self>) {
        if self.selected != Some(run_id) {
            self.selected = Some(run_id);
            self.show_prompt = false;
            self.load_log(run_id, cx);
        }
        cx.notify();
    }

    fn close_log(&mut self, cx: &mut Context<Self>) {
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
            .filter(|listing| listing.run.status == RunStatus::Succeeded)
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

    fn turns(&self, listing: &RunListing) -> Option<u32> {
        let live = self.live.get(&listing.run.id);
        listing
            .run
            .num_turns
            .or_else(|| live.and_then(|stats| stats.final_turns))
            .or_else(|| live.map(|stats| stats.turns).filter(|turns| *turns > 0))
    }

    fn cost(&self, listing: &RunListing) -> Option<f64> {
        listing.run.cost_usd.or_else(|| {
            self.live
                .get(&listing.run.id)
                .and_then(|stats| stats.cost_usd)
        })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let cost: f64 = self.runs.iter().filter_map(|l| self.cost(l)).sum();
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
            .when(self.repo_id.is_some(), |header| {
                header.child(
                    div()
                        .font_family(theme.fonts.code_family.clone())
                        .text_size(px(MONO_TEXT))
                        .text_color(colors.text_placeholder)
                        .child(self.runs.len().to_string()),
                )
            })
            .child(div().flex_1())
            .when(cost > 0.0, |header| {
                header.child(
                    div()
                        .font_family(theme.fonts.code_family.clone())
                        .text_size(px(MONO_TEXT))
                        .text_color(colors.text_placeholder)
                        .child(format_cost(cost)),
                )
            })
            .into_any_element()
    }

    /// One run card, with its share of the list's padding and gaps.
    fn render_card(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(listing) = self.runs.get(index) else {
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
        let style = card_style(status, colors);

        let snippet = listing
            .body_md
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .to_owned();
        let location = if comment_id.is_some() {
            format!("{}:{}", listing.file_path, listing.start_line)
        } else {
            listing.file_path.clone()
        };
        let mut meta = Vec::new();
        meta.push(if run.effort.is_empty() {
            run.model.clone()
        } else {
            format!("{} · {}", run.model, run.effort)
        });
        if let Some(turns) = self.turns(listing) {
            meta.push(format!("{turns} turn{}", if turns == 1 { "" } else { "s" }));
        }
        if let Some(cost) = self.cost(listing) {
            meta.push(format_cost(cost));
        }
        let elapsed = format_elapsed(elapsed_ms(run, now_ms()));
        let line_counts = (status == RunStatus::Succeeded)
            .then(|| self.line_counts.get(&id).copied().flatten())
            .flatten();

        let status_icon = match status {
            RunStatus::Running => claude_activity(px(14.0), cx),
            // Starting: the eye, still and muted (motion is for live work).
            RunStatus::Queued => {
                icon(IconName::ArgusEyeMono, px(14.0), style.label).into_any_element()
            }
            RunStatus::Succeeded => {
                icon(IconName::GitPullRequest, px(13.0), style.label).into_any_element()
            }
            RunStatus::Failed => icon(IconName::Warning, px(13.0), style.label).into_any_element(),
            RunStatus::NoChanges => icon(IconName::Check, px(12.0), style.label).into_any_element(),
            RunStatus::Stopped | RunStatus::Rejected => {
                icon(IconName::X, px(12.0), style.label).into_any_element()
            }
        };
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .child(status_icon)
            .child(
                div()
                    .flex_none()
                    .text_size(px(LABEL_TEXT))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(style.label)
                    .child(style.text),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .font_family(mono.clone())
                    .text_size(px(MONO_TEXT))
                    .text_color(style.secondary)
                    .child(elapsed),
            );
        let summary = div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .text_size(px(TITLE_TEXT))
            .text_color(style.summary)
            .child(snippet);
        let location_row = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .min_w_0()
            .font_family(mono.clone())
            .text_size(px(MONO_TEXT))
            .child(
                div()
                    .flex_none()
                    .max_w_full()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(style.secondary)
                    .child(location),
            )
            .when_some(line_counts, |row, (added, removed)| {
                row.child(
                    div()
                        .flex_none()
                        .text_color(colors.diff_added_text)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(colors.diff_removed_text)
                        .child(format!("−{removed}")),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(colors.text_placeholder)
                    .child(meta.join(" · ")),
            );

        let steps = (status == RunStatus::Running).then(|| {
            let steps = self
                .steps
                .get(&id)
                .map(|live| live.steps.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let inset = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded(radius::MD)
                .bg(colors.background)
                .font_family(mono.clone())
                .text_size(px(MONO_TEXT))
                .text_color(colors.text_muted);
            if steps.is_empty() {
                return inset
                    .child(
                        div()
                            .text_color(colors.text_placeholder)
                            .child("Waiting for Claude…"),
                    )
                    .into_any_element();
            }
            inset
                .children(steps.into_iter().map(|step| {
                    let (name, color) = match (step.finished, step.failed) {
                        (true, true) => (IconName::Warning, colors.error),
                        (true, false) => (IconName::Check, colors.text_muted),
                        (false, _) => (IconName::CaretRight, colors.text),
                    };
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .min_w_0()
                        .text_color(color)
                        .child(icon(name, px(11.0), color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(step.label),
                        )
                }))
                .into_any_element()
        });

        let stop = status.is_active().then(|| {
            let label = if status == RunStatus::Queued {
                "Cancel"
            } else {
                "Stop"
            };
            let control = if self.stopping.contains(&id) {
                disabled_button(("run-stop", index), "Stopping…", cx)
                    .h(px(22.0))
                    .text_size(px(LABEL_TEXT))
                    .into_any_element()
            } else {
                button(("run-stop", index), label, cx)
                    .h(px(22.0))
                    .text_size(px(LABEL_TEXT))
                    .text_color(colors.text_muted)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.stop(id, cx);
                    }))
                    .into_any_element()
            };
            // Ghost buttons pad their label; line it up with the text above.
            div().flex().ml(px(-8.0)).child(control)
        });

        let border = if selected {
            colors.accent
        } else {
            style.border
        };
        let card = div()
            .id(("run-card", index))
            .flex()
            .flex_col()
            .gap(px(if status == RunStatus::Running {
                8.0
            } else {
                6.0
            }))
            .px(px(12.0))
            .py(px(10.0))
            .min_w_0()
            .rounded(radius::LG)
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .when_some(style.fill, |card, fill| card.bg(fill))
            .when(style.fill.is_none(), |card| {
                card.hover(|style| style.bg(colors.element_hover))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select(id, cx);
                if let Some(comment_id) = comment_id {
                    cx.emit(RunPanelEvent::OpenComment(comment_id));
                }
            }))
            .child(header)
            .child(summary)
            .child(location_row)
            .children(steps)
            .children(stop);
        let last = index + 1 == self.runs.len();
        div()
            .px(px(LIST_PADDING))
            .when(index == 0, |item| item.pt(px(LIST_PADDING)))
            .pb(px(if last { LIST_PADDING } else { CARD_GAP }))
            .child(card)
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.runs.is_empty() {
            let text = if self.repo_id.is_none() {
                "No repository open."
            } else if self.list_task.is_some() {
                "Loading runs…"
            } else {
                "No runs yet. Comment on a line and run it with Claude (r)."
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

    fn render_detail(&self, view: &LogView, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let listing = self.listing(view.run_id);
        let title = listing
            .map(|listing| format!("{}:{}", listing.file_path, listing.start_line))
            .unwrap_or_else(|| "Run".to_owned());
        // A rejected run's worktree is gone.
        let worktree = listing
            .filter(|listing| listing.run.status != RunStatus::Rejected)
            .map(|listing| listing.run.worktree_path.clone());
        let proposal = listing
            .filter(|listing| listing.run.status == RunStatus::Succeeded)
            .map(|listing| listing.run.id);
        let toolbar =
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(30.0))
                .px_2()
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
                        .text_color(colors.text_muted)
                        .child(title),
                )
                .children(proposal.map(|run_id| {
                    button("run-view-proposal", "View proposal", cx)
                        .h(px(20.0))
                        .text_xs()
                        .text_color(colors.accent)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(RunPanelEvent::ViewProposal(run_id));
                        }))
                }))
                .child(
                    toggle_button("run-view-prompt", "View prompt", self.show_prompt, cx)
                        .h(px(20.0))
                        .text_xs()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_prompt = !this.show_prompt;
                            cx.notify();
                        })),
                )
                .children(worktree.clone().map(|path| {
                    button("run-reveal", reveal_label(), cx)
                        .h(px(20.0))
                        .text_xs()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_worktree(path.clone(), false, cx)
                        }))
                }))
                .children(worktree.map(|path| {
                    button("run-terminal", "Terminal", cx)
                        .h(px(20.0))
                        .text_xs()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_worktree(path.clone(), true, cx)
                        }))
                }))
                .child(
                    icon_button("run-close-log", IconName::X, "Close the log", cx)
                        .on_click(cx.listener(|this, _, _, cx| this.close_log(cx))),
                );

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
        // The log and the prompt fill the panel under the toolbar, on the
        // same background as a running card's steps, in the code font.
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
                .when(expanded, |row| {
                    row.child(detail_block(&text, colors, &code_font))
                })
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
                            .child(detail_block(&call.input, colors, &code_font))
                            .when_some(call.output.clone(), |row, output| {
                                row.child(detail_label("Output", colors)).child(
                                    detail_block(&output, colors, &code_font)
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
                        .rounded_md()
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
            .when(expanded, |row| {
                row.child(detail_block(&json, colors, &code_font))
            })
            .into_any_element(),
        }
    }
}

impl Render for RunPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let header = self.render_header(cx);
        let list = self.render_list(cx);
        let detail = self.log.as_ref().map(|view| self.render_detail(view, cx));
        let has_detail = detail.is_some();
        let list_pane = div().flex().flex_col().min_h_0().min_w_0().child(list);
        let list_pane = match (has_detail, self.horizontal) {
            (false, _) => list_pane.flex_1(),
            (true, true) => list_pane
                .h_full()
                .w(px(320.0))
                .flex_none()
                .border_r_1()
                .border_color(colors.border_variant),
            (true, false) => list_pane
                .flex_none()
                .h(px(LIST_HEIGHT_WITH_LOG))
                .border_b_1()
                .border_color(colors.border_variant),
        };
        let body = div()
            .flex()
            .flex_1()
            .min_h_0()
            .when(!self.horizontal, |body| body.flex_col())
            .child(list_pane)
            .children(detail);
        div()
            .id("run-panel")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(header)
            .child(body)
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
fn detail_block(text: &str, colors: &ThemeColors, font: &SharedString) -> gpui::Div {
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
        .rounded_md()
        .bg(colors.surface)
        .font_family(font.clone())
        .text_xs()
        .text_color(colors.text)
        .child(shown)
}

/// How a run card looks in each state (Panoptes #56 §3, §5).
struct CardStyle {
    text: &'static str,
    /// Status label and icon.
    label: Hsla,
    border: Hsla,
    fill: Option<Hsla>,
    /// The comment summary.
    summary: Hsla,
    /// `file:line` and the elapsed time.
    secondary: Hsla,
}

fn card_style(status: RunStatus, colors: &ThemeColors) -> CardStyle {
    let quiet = |text, label| CardStyle {
        text,
        label,
        border: colors.border_variant,
        fill: None,
        summary: colors.text_muted,
        secondary: colors.text_placeholder,
    };
    match status {
        RunStatus::Queued => CardStyle {
            summary: colors.text,
            secondary: colors.text_muted,
            ..quiet("Starting", colors.text_muted)
        },
        RunStatus::Running => CardStyle {
            text: "Running",
            label: colors.claude_text,
            border: colors.claude_border,
            fill: Some(colors.claude_soft),
            summary: colors.text,
            secondary: colors.text_muted,
        },
        RunStatus::Succeeded => CardStyle {
            text: "Ready to review",
            label: colors.accent,
            border: colors.accent_border,
            fill: None,
            summary: colors.text,
            secondary: colors.text_muted,
        },
        RunStatus::Failed => CardStyle {
            text: "Failed",
            label: colors.error,
            border: colors.border,
            fill: None,
            summary: colors.text,
            secondary: colors.text_muted,
        },
        RunStatus::NoChanges => quiet("No changes", colors.text_muted),
        RunStatus::Stopped => quiet("Stopped", colors.text_placeholder),
        RunStatus::Rejected => quiet("Rejected", colors.text_placeholder),
    }
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
