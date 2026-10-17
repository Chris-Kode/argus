//! Comment runs: one `claude -p` process per run, each in its own worktree
//! (spec §6.3).
//!
//! [`start`] records the run, creates its worktree at the head tip, builds the
//! prompt and the command, moves the comment to `Running` and spawns the
//! process. From then on an independent tokio task ([`supervise`]) owns the
//! child: it parses stdout line by line, queues each event for the batched
//! `run_events` insert, publishes it to subscribers, honors Stop, and on exit
//! computes the proposal patch and writes the outcome. There is no queue and
//! no limit: every run is its own task, and nothing ever waits on one from
//! the UI thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use argus_diff::review::{
    Comment, CommentEvent, ReviewError, apply_event, transition as comment_transition,
};
use argus_diff::{DiffError, DiffOptions, DiffSession, LoadMode};
use argus_git::{GitError, GitService, Sha, Worktree, WorktreeManager};
use argus_store::{
    CommentId, NewRun, ReplyAuthor, RunId, RunKind, RunOutcome, RunStatus, StorageService,
    StoreError,
};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Child;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;
use uuid::Uuid;

use crate::claude::{
    AUTH_ERROR_HINT, ClaudeCapabilities, ClaudeCommand, SessionMode, is_auth_error,
};
use crate::prompt::{CommentContext, PromptError, RepoContext, comment_prompt};
use crate::stream::{RunResult, StreamEvent, parse_line};

/// How long Stop waits after `SIGTERM` before it sends `SIGKILL`.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How much of stderr a failed run keeps in its `error`, in bytes.
pub const STDERR_TAIL_BYTES: usize = 4 * 1024;

/// Buffered lifecycle updates per subscriber.
const STATUS_CAPACITY: usize = 256;

/// Buffered stream events per subscriber. A subscriber that lags behind gets
/// `RecvError::Lagged` and can replay the run from `run_events`.
const EVENT_CAPACITY: usize = 8192;

/// Stdout lines read ahead of the parser.
const LINE_BUFFER: usize = 256;

/// Longest prefix of a malformed line written to the log, in bytes.
const MALFORMED_LOG_BYTES: usize = 200;

/// Why a run could not start.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// `claude` is missing, still being detected, or too old.
    #[error("{0}")]
    ClaudeUnavailable(String),
    /// The comment does not exist.
    #[error("comment {0} does not exist")]
    CommentNotFound(CommentId),
    /// The comment's repository is not recorded.
    #[error("the repository of comment {0} is not recorded")]
    RepoNotFound(CommentId),
    /// The comment cannot start a run in its current state.
    #[error(transparent)]
    Review(#[from] ReviewError),
    /// Reading the diff for the prompt failed.
    #[error(transparent)]
    Diff(#[from] DiffError),
    /// A git operation (opening the repo, creating the worktree) failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// The prompt could not be rendered.
    #[error(transparent)]
    Prompt(#[from] PromptError),
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The `claude` process could not be started.
    #[error("cannot start claude: {0}")]
    Spawn(String),
}

/// A run's lifecycle change, for views that show statuses.
#[derive(Clone, Debug, PartialEq)]
pub enum RunUpdate {
    /// The run's process was spawned; its comment is now `Running`.
    Started {
        /// The run.
        run_id: RunId,
        /// The comment it works on.
        comment_id: Option<CommentId>,
    },
    /// The run reached a terminal status, stored with its comment's new
    /// status.
    Finished(RunFinished),
    /// The user rejected the run's proposal: its worktree is gone, the run
    /// is `Rejected` and its comment back to `Open`.
    Rejected {
        /// The run.
        run_id: RunId,
        /// The comment it worked on.
        comment_id: Option<CommentId>,
    },
}

/// The outcome of a finished run, as published.
#[derive(Clone, Debug, PartialEq)]
pub struct RunFinished {
    /// The run.
    pub run_id: RunId,
    /// The comment it worked on.
    pub comment_id: Option<CommentId>,
    /// Terminal status.
    pub status: RunStatus,
    /// Claude's final message.
    pub summary: Option<String>,
    /// Why the run failed.
    pub error: Option<String>,
}

/// One stream-json event of a live run, in stream order (`seq`).
#[derive(Clone, Debug)]
pub struct RunStreamEvent {
    /// The run.
    pub run_id: RunId,
    /// Position in the run's stream, as stored in `run_events.seq`.
    pub seq: i64,
    /// The parsed event.
    pub event: Arc<StreamEvent>,
}

/// The services a run needs, shared by every run of the manager.
#[derive(Debug)]
pub(crate) struct Services {
    pub store: StorageService,
    pub git: GitService,
    pub worktrees: WorktreeManager,
    status: broadcast::Sender<RunUpdate>,
    events: broadcast::Sender<RunStreamEvent>,
    active: Mutex<HashMap<RunId, ActiveRun>>,
}

#[derive(Debug)]
struct ActiveRun {
    stop: watch::Sender<bool>,
}

impl Services {
    pub fn new(store: StorageService, git: GitService, worktrees: WorktreeManager) -> Self {
        let (status, _) = broadcast::channel(STATUS_CAPACITY);
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            store,
            git,
            worktrees,
            status,
            events,
            active: Mutex::new(HashMap::new()),
        }
    }

    pub fn subscribe_status(&self) -> broadcast::Receiver<RunUpdate> {
        self.status.subscribe()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<RunStreamEvent> {
        self.events.subscribe()
    }

    pub fn active_runs(&self) -> Vec<RunId> {
        self.lock_active().keys().copied().collect()
    }

    pub fn is_active(&self, run_id: RunId) -> bool {
        self.lock_active().contains_key(&run_id)
    }

    /// Asks the run to stop; `false` when it is not running in this session.
    pub fn stop(&self, run_id: RunId) -> bool {
        match self.lock_active().get(&run_id) {
            Some(run) => {
                run.stop.send_replace(true);
                tracing::info!(run = %run_id, "stop requested");
                true
            }
            None => false,
        }
    }

    pub(crate) fn publish(&self, update: RunUpdate) {
        // No subscriber is fine: nothing is showing runs.
        let _ = self.status.send(update);
    }

    fn lock_active(&self) -> MutexGuard<'_, HashMap<RunId, ActiveRun>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What [`start`] needs besides the comment.
pub(crate) struct StartRequest {
    pub comment_id: CommentId,
    pub model: String,
    pub effort: String,
    pub capabilities: ClaudeCapabilities,
    pub rules_path: PathBuf,
}

/// Starts a comment run and returns once its process is spawned.
///
/// Errors before the run row exists leave nothing behind. Once it exists, a
/// failure is recorded on the run (`Failed` with the reason), the comment
/// goes back to `Open`, and the error is returned as well.
pub(crate) async fn start(
    services: Arc<Services>,
    request: StartRequest,
) -> Result<RunId, RunError> {
    let StartRequest {
        comment_id,
        model,
        effort,
        capabilities,
        rules_path,
    } = request;
    let store = &services.store;
    let stored = store
        .comments()
        .get(comment_id)
        .await?
        .ok_or(RunError::CommentNotFound(comment_id))?;
    // Refuse early, before any git work; `apply_event` checks again below.
    if stored.outdated {
        return Err(ReviewError::Outdated(comment_id).into());
    }
    comment_transition(stored.status, CommentEvent::RunStarted).map_err(ReviewError::from)?;
    let record = store
        .repos()
        .get(stored.repo_id)
        .await?
        .ok_or(RunError::RepoNotFound(comment_id))?;
    let repo = services.git.open(record.path.clone()).await?;

    // The worktree starts at the head tip as it is now (spec §6.3).
    let session = DiffSession::new(
        &repo,
        &stored.base_ref,
        &stored.head_ref,
        DiffOptions::default(),
    )
    .await?;
    let start_sha = session.head();
    let comment = Comment::from_stored(stored, Vec::new());
    let file_diff = match session
        .file_diff(&comment.anchor.file_path, LoadMode::Normal)
        .await
    {
        Ok(diff) => Some(diff),
        Err(error) => {
            // The prompt falls back to the snippet saved with the comment.
            tracing::debug!(comment = %comment_id, %error, "no line diff for the prompt");
            None
        }
    };
    let repo_context = RepoContext {
        repo_name: record.name.clone(),
        base_branch: comment.base_ref.clone(),
        head_branch: comment.head_ref.clone(),
        start_sha: start_sha.to_hex(),
    };
    // Building the excerpt walks the file diff: keep it off the async workers.
    let prompt = tokio::task::spawn_blocking(move || {
        let context =
            CommentContext::from_comment(&comment, file_diff.as_ref().and_then(|d| d.text()));
        comment_prompt(&repo_context, &[context])
    })
    .await
    .map_err(|error| RunError::Spawn(error.to_string()))??;

    // 1. The run row (`kind = comment`, a new session for the thread).
    let run_id = RunId::new();
    let session_id = Uuid::now_v7();
    let worktree_path = services.worktrees.run_path(repo.id(), run_id);
    store
        .runs()
        .create_with_id(
            run_id,
            NewRun {
                comment_id: Some(comment_id),
                kind: RunKind::Comment,
                session_id: session_id.hyphenated().to_string(),
                model: model.clone(),
                effort: effort.clone(),
                worktree_path,
                start_sha: start_sha.to_hex(),
                prompt: prompt.text.clone(),
            },
        )
        .await?;

    // The comment moves to `Running` before any expensive work, so a second
    // start of the same comment is refused by the compare-and-swap.
    if let Err(error) = apply_event(&store.comments(), comment_id, CommentEvent::RunStarted).await {
        if let Err(delete_error) = store.runs().delete(run_id).await {
            tracing::warn!(run = %run_id, error = %delete_error, "cannot delete unstarted run");
        }
        return Err(error.into());
    }

    let fail = |reason: String| {
        let services = services.clone();
        async move {
            let mut outcome = RunOutcome::new(RunStatus::Failed);
            outcome.error = Some(reason.clone());
            record_finish(&services, run_id, Some(comment_id), outcome).await;
        }
    };

    // 2. The worktree at the head tip.
    let worktree = match services
        .worktrees
        .create_run_worktree(&repo, &start_sha, run_id)
        .await
    {
        Ok(worktree) => worktree,
        Err(error) => {
            fail(format!("cannot create the run worktree: {error}")).await;
            return Err(error.into());
        }
    };

    // 3. The command, spawned in the worktree.
    let effort_arg = (!effort.trim().is_empty()).then_some(effort.as_str());
    let command = ClaudeCommand::review_run(
        worktree.path(),
        prompt.text,
        model,
        effort_arg,
        SessionMode::New(session_id),
        rules_path,
    )
    .adapt_to(&capabilities);
    let mut child = match command.to_command(capabilities.path()).spawn() {
        Ok(child) => child,
        Err(error) => {
            let reason = format!("cannot start {}: {error}", capabilities.path().display());
            fail(reason.clone()).await;
            return Err(RunError::Spawn(reason));
        }
    };
    if let (Some(bytes), Some(mut stdin)) =
        (command.stdin().map(<[u8]>::to_vec), child.stdin.take())
    {
        // A long prompt can exceed the pipe buffer; write it without blocking
        // the start, then close stdin so Claude sees the end of the prompt.
        tokio::spawn(async move {
            if let Err(error) = stdin.write_all(&bytes).await {
                tracing::warn!(run = %run_id, %error, "cannot write the prompt to claude");
            }
        });
    }

    // 4. Running; register for Stop before anyone can see the run.
    if let Err(error) = store.runs().set_status(run_id, RunStatus::Running).await {
        tracing::warn!(run = %run_id, %error, "cannot mark the run as running");
    }
    let (stop_tx, stop_rx) = watch::channel(false);
    services
        .lock_active()
        .insert(run_id, ActiveRun { stop: stop_tx });
    tracing::info!(
        run = %run_id,
        comment = %comment_id,
        worktree = %worktree.path().display(),
        start_sha = %start_sha,
        "started run"
    );
    services.publish(RunUpdate::Started {
        run_id,
        comment_id: Some(comment_id),
    });
    tokio::spawn(supervise(
        services.clone(),
        LiveRun {
            run_id,
            comment_id,
            worktree,
            start_sha,
        },
        child,
        stop_rx,
    ));
    Ok(run_id)
}

/// A spawned run, as its supervisor sees it.
struct LiveRun {
    run_id: RunId,
    comment_id: CommentId,
    worktree: Worktree,
    start_sha: Sha,
}

/// What the stream told us so far.
#[derive(Default)]
struct StreamState {
    result: Option<RunResult>,
    last_text: Option<String>,
    malformed: usize,
}

/// Owns a run's process from spawn to its stored outcome.
async fn supervise(
    services: Arc<Services>,
    run: LiveRun,
    mut child: Child,
    mut stop: watch::Receiver<bool>,
) {
    let started = Instant::now();
    let run_id = run.run_id;
    let pid = child.id();
    let stderr = tokio::spawn(read_tail(child.stderr.take()));
    let (line_tx, mut lines) = mpsc::channel::<Vec<u8>>(LINE_BUFFER);
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(read_lines(stdout, line_tx, run_id));
    }

    let runs = services.store.runs();
    let mut state = StreamState::default();
    let mut seq: i64 = 0;
    let mut store_failed = false;
    let mut stopping = false;
    let mut stop_closed = false;
    let mut kill_at: Option<Instant> = None;
    let mut stdout_open = true;

    let exit = loop {
        tokio::select! {
            changed = stop.changed(), if !stopping && !stop_closed => {
                // A closed channel means the run was unregistered; nothing
                // can ask for a stop any more.
                if changed.is_err() {
                    stop_closed = true;
                    continue;
                }
                if !*stop.borrow_and_update() {
                    continue;
                }
                stopping = true;
                if let Some(pid) = pid {
                    signal_group(pid, Signal::Term);
                    kill_at = Some(Instant::now() + STOP_GRACE);
                }
            }
            () = sleep_until(kill_at), if kill_at.is_some() => {
                kill_at = None;
                if let Some(pid) = pid {
                    tracing::info!(run = %run_id, "claude ignored SIGTERM; sending SIGKILL");
                    signal_group(pid, Signal::Kill);
                }
            }
            line = lines.recv(), if stdout_open => match line {
                Some(line) => handle_line(&services, &runs, run_id, &line, &mut seq, &mut state, &mut store_failed),
                None => stdout_open = false,
            },
            status = child.wait(), if !stdout_open => break status,
        }
    };
    let stopped = stopping;
    let stderr_tail = stderr.await.unwrap_or_default();
    let wall_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    if state.malformed > 0 {
        tracing::warn!(run = %run_id, count = state.malformed, "run printed malformed stream-json lines");
    }

    let outcome = match exit {
        Ok(exit) => {
            complete(
                &services,
                &run,
                exit,
                stopped,
                &state,
                &stderr_tail,
                wall_ms,
            )
            .await
        }
        Err(error) => {
            let mut outcome = RunOutcome::new(RunStatus::Failed);
            outcome.error = Some(format!("cannot wait for claude: {error}"));
            outcome
        }
    };
    record_finish(&services, run_id, Some(run.comment_id), outcome).await;
}

/// Parses, stores and publishes one stdout line.
fn handle_line(
    services: &Services,
    runs: &argus_store::RunStore,
    run_id: RunId,
    line: &[u8],
    seq: &mut i64,
    state: &mut StreamState,
    store_failed: &mut bool,
) {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end_matches(['\n', '\r']);
    if text.trim().is_empty() {
        return;
    }
    let event = match parse_line(text) {
        Ok(event) => event,
        Err(error) => {
            // Never fatal: Claude Code may print a stray non-JSON line.
            state.malformed += 1;
            tracing::warn!(
                run = %run_id,
                %error,
                line = %prefix(text, MALFORMED_LOG_BYTES),
                "skipping malformed stream-json line"
            );
            return;
        }
    };
    match &event {
        StreamEvent::Result(result) => state.result = Some(result.clone()),
        StreamEvent::Assistant(message) => {
            if let Some(text) = message.text() {
                state.last_text = Some(text);
            }
        }
        _ => {}
    }
    if let Err(error) = runs.append_event(run_id, *seq, event.event_type(), text)
        && !*store_failed
    {
        tracing::error!(run = %run_id, %error, "cannot store run events");
        *store_failed = true;
    }
    let _ = services.events.send(RunStreamEvent {
        run_id,
        seq: *seq,
        event: Arc::new(event),
    });
    *seq += 1;
}

/// The terminal outcome of an exited run.
async fn complete(
    services: &Services,
    run: &LiveRun,
    exit: ExitStatus,
    stopped: bool,
    state: &StreamState,
    stderr_tail: &str,
    wall_ms: i64,
) -> RunOutcome {
    let result = state.result.as_ref();
    let mut outcome = RunOutcome::new(RunStatus::Failed);
    outcome.exit_code = exit.code();
    outcome.cost_usd = result.and_then(|r| r.total_cost_usd);
    outcome.num_turns = result.and_then(|r| r.num_turns);
    outcome.duration_ms = result.and_then(|r| r.duration_ms).or(Some(wall_ms));
    outcome.summary = result
        .and_then(|r| r.result.clone())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| state.last_text.clone());

    if stopped {
        // The worktree stays: the partial diff can still be reviewed.
        outcome.status = RunStatus::Stopped;
        return outcome;
    }
    if !exit.success() || result.is_some_and(|r| r.is_error) {
        outcome.error = Some(failure_reason(exit, result, stderr_tail));
        return outcome;
    }
    match services
        .worktrees
        .diff_against_start(&run.worktree, &run.start_sha)
        .await
    {
        Ok(patch) if patch.is_empty() => outcome.status = RunStatus::NoChanges,
        Ok(patch) => {
            tracing::info!(run = %run.run_id, files = patch.files.len(), "run proposed a change");
            outcome.status = RunStatus::Succeeded;
        }
        Err(error) => {
            outcome.error = Some(format!("cannot compute the proposed change: {error}"));
        }
    }
    outcome
}

/// A clear failure reason: the `result` subtype and message (or the exit
/// status), a sign-in hint when relevant, and the stderr tail.
fn failure_reason(exit: ExitStatus, result: Option<&RunResult>, stderr_tail: &str) -> String {
    let message = result
        .and_then(|r| r.result.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let mut reason = match result {
        Some(result) if result.is_error => {
            // Claude Code reports some errors (e.g. a missing sign-in) with
            // subtype `success`; only a real error subtype is worth naming.
            let what = match result.subtype.as_str() {
                "" | "success" => "an error".to_owned(),
                subtype => format!("an error ({subtype})"),
            };
            match message {
                Some(message) => format!("Claude reported {what}: {message}"),
                None => format!("Claude reported {what}"),
            }
        }
        Some(_) => format!("claude exited with {exit}"),
        None => format!("claude exited with {exit} before reporting a result"),
    };
    if is_auth_error(message.unwrap_or_default()) || is_auth_error(stderr_tail) {
        reason.push_str(&format!(". {AUTH_ERROR_HINT}."));
    }
    let tail = stderr_tail.trim();
    if !tail.is_empty() {
        reason.push_str("\n\nstderr:\n");
        reason.push_str(tail);
    }
    reason
}

/// Writes the outcome, moves the comment on, and publishes the result.
async fn record_finish(
    services: &Services,
    run_id: RunId,
    comment_id: Option<CommentId>,
    outcome: RunOutcome,
) {
    let status = outcome.status;
    let finished = RunFinished {
        run_id,
        comment_id,
        status,
        summary: outcome.summary.clone(),
        error: outcome.error.clone(),
    };
    // `finish` is a storage job, so every queued event is written first.
    if let Err(error) = services.store.runs().finish(run_id, outcome).await {
        tracing::error!(run = %run_id, %error, "cannot store the run outcome");
    }
    if let Some(comment_id) = comment_id
        && let Some(event) = CommentEvent::from_run_status(status)
    {
        let comments = services.store.comments();
        if let Err(error) = apply_event(&comments, comment_id, event).await {
            tracing::warn!(run = %run_id, comment = %comment_id, %error, "cannot update the comment status");
        }
        // Nothing to review: show Claude's final message in the thread.
        if status == RunStatus::NoChanges
            && let Some(summary) = finished.summary.clone()
            && let Err(error) = comments
                .add_reply(comment_id, ReplyAuthor::Claude, summary)
                .await
        {
            tracing::warn!(run = %run_id, %error, "cannot add Claude's reply");
        }
    }
    services.lock_active().remove(&run_id);
    tracing::info!(run = %run_id, %status, error = ?finished.error, "run finished");
    services.publish(RunUpdate::Finished(finished));
}

/// Forwards stdout lines (with their newline) until EOF.
async fn read_lines(
    stdout: tokio::process::ChildStdout,
    lines: mpsc::Sender<Vec<u8>>,
    run_id: RunId,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => return,
            Ok(_) => {
                if lines.send(line).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                tracing::warn!(run = %run_id, %error, "cannot read claude's output");
                return;
            }
        }
    }
}

/// Reads stderr to the end and keeps its last [`STDERR_TAIL_BYTES`].
async fn read_tail(stderr: Option<tokio::process::ChildStderr>) -> String {
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                tail.extend_from_slice(&chunk[..read]);
                if tail.len() > 2 * STDERR_TAIL_BYTES {
                    tail.drain(..tail.len() - STDERR_TAIL_BYTES);
                }
            }
        }
    }
    if tail.len() > STDERR_TAIL_BYTES {
        tail.drain(..tail.len() - STDERR_TAIL_BYTES);
    }
    String::from_utf8_lossy(&tail).into_owned()
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[derive(Clone, Copy, Debug)]
enum Signal {
    Term,
    Kill,
}

/// Signals the run's whole process group: `claude` was spawned as the leader
/// of its own group (see [`ClaudeCommand::to_command`]), so its tools and
/// their children stop with it.
#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    let number = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: `kill` takes plain integers and has no memory effects. The group
    // id is the child's pid, which stays reserved until the child is reaped,
    // and the supervisor only signals before it reaps the child.
    let result = unsafe { libc::kill(-pid, number) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        tracing::debug!(pid, ?signal, %error, "cannot signal the run's process group");
    }
}

/// Process groups are Unix-only; elsewhere Stop cannot signal the tree.
#[cfg(not(unix))]
fn signal_group(pid: u32, signal: Signal) {
    tracing::warn!(pid, ?signal, "stopping runs is only supported on Unix");
}
