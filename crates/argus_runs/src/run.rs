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
//!
//! [`follow_up`] continues a `Proposed` comment: a new run (`kind =
//! followup`) resumes the thread's Claude session (`--resume`) in the
//! worktree of the current proposal, so the next proposal combines both
//! runs (`start_sha → worktree`). When the session cannot be resumed, the
//! run fails with [`SESSION_NOT_FOUND`] in its error, and a fresh follow-up
//! ([`FollowupMode::Fresh`]) starts a new session with the whole thread in
//! its prompt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use argus_diff::review::{
    Comment, CommentEvent, ReviewError, apply_event, transition as comment_transition,
};
use argus_diff::{DiffError, DiffOptions, DiffSession, LoadMode};
use argus_git::{GitError, GitService, Repo, Sha, Worktree, WorktreeManager};
use argus_store::{
    CommentId, CommentStatus, NewRun, ReplyAuthor, RunId, RunKind, RunOutcome, RunStatus,
    StorageService, StoreError,
};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Child;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::Instant;
use uuid::Uuid;

use crate::claude::{
    AUTH_ERROR_HINT, ClaudeCapabilities, ClaudeCommand, SessionMode, is_auth_error,
};
use crate::prompt::{
    CommentContext, Prompt, PromptError, RepoContext, ThreadMessage, comment_prompt,
    followup_prompt, fresh_followup_prompt,
};
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

/// Starts the error of a follow-up whose Claude session could not be
/// resumed; [`is_session_not_found`] recognizes it in a stored run.
pub const SESSION_NOT_FOUND: &str = "Claude could not resume this comment's session";

/// Lower-case fragments of what Claude Code prints when `--resume` names a
/// session it does not have.
const SESSION_NOT_FOUND_PATTERNS: [&str; 3] = [
    "no conversation found",
    "session not found",
    "no session found",
];

/// Whether a run's stored `error` says its session could not be resumed,
/// so the UI can offer a fresh follow-up with the thread as context.
pub fn is_session_not_found(error: &str) -> bool {
    error.starts_with(SESSION_NOT_FOUND)
}

/// Whether Claude's output (stderr, or a failed `result` text) reports a
/// missing session.
fn reports_missing_session(text: &str) -> bool {
    let text = text.to_lowercase();
    SESSION_NOT_FOUND_PATTERNS
        .iter()
        .any(|pattern| text.contains(pattern))
}

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
    /// The comment has no proposal whose worktree still exists (e.g. it was
    /// rejected), so there is nothing to follow up on; a new run is needed.
    #[error(
        "comment {0} has no proposal to follow up on (its worktree is gone); start a new run instead"
    )]
    FollowupUnavailable(CommentId),
    /// A run of the comment is still running.
    #[error("comment {0} already has a run in progress")]
    Busy(CommentId),
    /// The stored session id of the thread is not a UUID.
    #[error("the Claude session of comment {0} is not a valid id: `{1}`")]
    InvalidSession(CommentId, String),
    /// The run or proposal a resolve run starts from cannot be loaded.
    #[error(transparent)]
    Proposal(#[from] crate::proposal::ProposalError),
    /// The conflict to resolve no longer happens (the head or the upstream
    /// moved); nothing was started.
    #[error("{0}")]
    ConflictGone(String),
    /// The pull-before-push resolution is no longer in progress: it was
    /// cancelled, it failed, or Argus restarted.
    #[error(
        "this pull-before-push resolution is no longer in progress (cancelled, failed, or Argus \
         restarted); Push again to start over"
    )]
    ResolveGone,
    /// The run has no reviewable proposal to follow up on.
    #[error("run {0} has no proposal to follow up on")]
    RunNotReviewable(RunId),
    /// A run is already working in the worktree.
    #[error("a run is already working in this worktree; wait for it or stop it")]
    RunBusy,
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
    /// The run's proposal was accepted: the commit is on the head branch,
    /// the comment is `Resolved` and the run's worktree is gone.
    Accepted {
        /// The run.
        run_id: RunId,
        /// The comment it worked on.
        comment_id: Option<CommentId>,
        /// The new head tip.
        commit: Sha,
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
    /// Why the run was started.
    pub kind: RunKind,
    /// The run's `--resume` failed: the thread's session is gone. The UI
    /// offers a fresh follow-up with the thread as context.
    pub session_not_found: bool,
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
    /// Pull-before-push resolutions in progress.
    pub resolves: crate::resolve::ResolveSessions,
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
            resolves: crate::resolve::ResolveSessions::default(),
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

/// How a follow-up talks to Claude.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowupMode {
    /// Resume the thread's session with the reviewer's reply as the prompt.
    /// The reply is saved in the thread when the run starts.
    Resume {
        /// The reply text.
        reply: String,
    },
    /// Start a new session whose prompt carries the comment and the whole
    /// thread, for a thread whose session could not be resumed.
    Fresh,
}

/// What [`follow_up`] needs besides the comment.
pub(crate) struct FollowupRequest {
    pub comment_id: CommentId,
    pub mode: FollowupMode,
    pub model: String,
    pub effort: String,
    pub capabilities: ClaudeCapabilities,
    pub rules_path: PathBuf,
}

/// The stored comment with its repository, checked for a new run.
pub(crate) struct Target {
    pub stored: argus_store::Comment,
    pub repo_name: String,
    pub repo: Repo,
}

async fn load_target(services: &Services, comment_id: CommentId) -> Result<Target, RunError> {
    load_comment_target(services, comment_id, false).await
}

/// The comment and its repository; `allow_outdated` lets an outdated
/// comment through (a resolve run continues its existing proposal).
pub(crate) async fn load_comment_target(
    services: &Services,
    comment_id: CommentId,
    allow_outdated: bool,
) -> Result<Target, RunError> {
    let store = &services.store;
    let stored = store
        .comments()
        .get(comment_id)
        .await?
        .ok_or(RunError::CommentNotFound(comment_id))?;
    // Refuse early, before any git work; `apply_event` checks again later.
    if stored.outdated && !allow_outdated {
        return Err(ReviewError::Outdated(comment_id).into());
    }
    let record = store
        .repos()
        .get(stored.repo_id)
        .await?
        .ok_or(RunError::RepoNotFound(comment_id))?;
    let repo = services.git.open(record.path.clone()).await?;
    Ok(Target {
        stored,
        repo_name: record.name,
        repo,
    })
}

/// The comment as the prompt shows it, from the current diff of its review,
/// and the head tip that diff ends at.
pub(crate) async fn comment_context(
    target: &Target,
) -> Result<(Sha, impl FnOnce() -> CommentContext + Send + 'static), RunError> {
    let stored = &target.stored;
    let session = DiffSession::new(
        &target.repo,
        &stored.base_ref,
        &stored.head_ref,
        DiffOptions::default(),
    )
    .await?;
    let head = session.head();
    let comment = Comment::from_stored(stored.clone(), Vec::new());
    let file_diff = match session
        .file_diff(&comment.anchor.file_path, LoadMode::Normal)
        .await
    {
        Ok(diff) => Some(diff),
        Err(error) => {
            // The prompt falls back to the snippet saved with the comment.
            tracing::debug!(comment = %stored.id, %error, "no line diff for the prompt");
            None
        }
    };
    // Building the excerpt walks the file diff: callers run this off the
    // async workers.
    let build =
        move || CommentContext::from_comment(&comment, file_diff.as_ref().and_then(|d| d.text()));
    Ok((head, build))
}

pub(crate) fn repo_context(target: &Target, start_sha: &str) -> RepoContext {
    RepoContext {
        repo_name: target.repo_name.clone(),
        base_branch: target.stored.base_ref.clone(),
        head_branch: target.stored.head_ref.clone(),
        start_sha: start_sha.to_owned(),
    }
}

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RunError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| RunError::Spawn(error.to_string()))
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
    let target = load_target(&services, comment_id).await?;
    comment_transition(target.stored.status, CommentEvent::RunStarted)
        .map_err(ReviewError::from)?;

    // The worktree starts at the head tip as it is now (spec §6.3).
    let (start_sha, build_context) = comment_context(&target).await?;
    let repo_context = repo_context(&target, &start_sha.to_hex());
    let prompt = blocking(move || comment_prompt(&repo_context, &[build_context()])).await??;

    // 1. The run row (`kind = comment`, a new session for the thread).
    let run_id = RunId::new();
    let session_id = Uuid::now_v7();
    let worktree_path = services.worktrees.run_path(target.repo.id(), run_id);
    let pending = PendingRun {
        run_id,
        comment_id: Some(comment_id),
        kind: RunKind::Comment,
        start_sha,
        model,
        effort,
        session: SessionMode::New(session_id),
        capabilities,
        rules_path,
    };
    record_run(&services, &pending, worktree_path, &prompt).await?;
    claim_comment(&services, &pending, CommentEvent::RunStarted).await?;

    // 2. The worktree at the head tip.
    let worktree = match services
        .worktrees
        .create_run_worktree(&target.repo, &start_sha, run_id)
        .await
    {
        Ok(worktree) => worktree,
        Err(error) => {
            let reason = format!("cannot create the run worktree: {error}");
            fail_unstarted(&services, &pending, None, reason).await;
            return Err(error.into());
        }
    };

    // 3. The command, spawned in the worktree.
    spawn(services, pending, worktree, prompt.text).await
}

/// Starts a follow-up of a `Proposed` comment and returns once its process
/// is spawned (spec §6.2).
///
/// The run (`kind = followup`) works in the worktree of the comment's
/// current proposal and keeps its `start_sha`, so its proposal combines the
/// changes of every run of the thread. [`FollowupMode::Resume`] resumes the
/// thread's session; [`FollowupMode::Fresh`] starts a new one.
///
/// A follow-up that fails, stops or changes nothing leaves the comment
/// `Proposed` with the change already in the worktree.
pub(crate) async fn follow_up(
    services: Arc<Services>,
    request: FollowupRequest,
) -> Result<RunId, RunError> {
    let FollowupRequest {
        comment_id,
        mode,
        model,
        effort,
        capabilities,
        rules_path,
    } = request;
    let target = load_target(&services, comment_id).await?;
    let event = match (&mode, target.stored.status) {
        // A fresh follow-up may also continue a thread that went back to
        // Open, as long as its worktree is still there.
        (FollowupMode::Fresh, CommentStatus::Open) => CommentEvent::RunStarted,
        _ => CommentEvent::FollowUp,
    };
    comment_transition(target.stored.status, event).map_err(|error| {
        match target.stored.status {
            CommentStatus::Running => RunError::Busy(comment_id),
            // Rejected (or never proposed): no proposal to continue.
            CommentStatus::Open => RunError::FollowupUnavailable(comment_id),
            _ => RunError::Review(error.into()),
        }
    })?;

    // The previous run whose worktree carries on.
    let runs = services.store.runs().list_for_comment(comment_id).await?;
    let previous = runs
        .iter()
        .rev()
        .find(|run| match mode {
            FollowupMode::Resume { .. } => {
                matches!(run.status, RunStatus::Succeeded | RunStatus::Stopped)
            }
            FollowupMode::Fresh => run.status != RunStatus::Rejected,
        })
        .filter(|run| run.worktree_path.is_dir())
        .cloned()
        .ok_or(RunError::FollowupUnavailable(comment_id))?;
    if runs.iter().any(|run| {
        run.status.is_active()
            && services.is_active(run.id)
            && same_path(&run.worktree_path, &previous.worktree_path)
    }) {
        return Err(RunError::Busy(comment_id));
    }
    let start_sha = Sha::parse(&previous.start_sha).ok_or_else(|| GitError::Parse {
        command: "runs.start_sha".to_owned(),
        message: format!("not an object id: `{}`", previous.start_sha),
    })?;
    let worktree = services
        .worktrees
        .worktree_at(&target.repo, previous.worktree_path.clone())?;

    let (session, prompt, reply) = match mode {
        FollowupMode::Resume { reply } => {
            let session = Uuid::parse_str(previous.session_id.trim())
                .map_err(|_| RunError::InvalidSession(comment_id, previous.session_id.clone()))?;
            (
                SessionMode::Resume(session),
                followup_prompt(&reply),
                Some(reply),
            )
        }
        FollowupMode::Fresh => {
            let thread: Vec<ThreadMessage> = services
                .store
                .comments()
                .replies(comment_id)
                .await?
                .into_iter()
                .map(|reply| ThreadMessage {
                    author: reply.author,
                    body: reply.body_md,
                })
                .collect();
            let (_, build_context) = comment_context(&target).await?;
            let repo_context = repo_context(&target, &previous.start_sha);
            let prompt =
                blocking(move || fresh_followup_prompt(&repo_context, &build_context(), &thread))
                    .await?;
            (SessionMode::New(Uuid::now_v7()), prompt, None)
        }
    };

    let pending = PendingRun {
        run_id: RunId::new(),
        comment_id: Some(comment_id),
        kind: RunKind::Followup,
        start_sha,
        model,
        effort,
        session,
        capabilities,
        rules_path,
    };
    record_run(&services, &pending, previous.worktree_path.clone(), &prompt).await?;
    claim_comment(&services, &pending, event).await?;
    if let Some(reply) = reply
        && let Err(error) = services
            .store
            .comments()
            .add_reply(comment_id, ReplyAuthor::User, reply)
            .await
    {
        let reason = format!("cannot save the reply: {error}");
        fail_unstarted(&services, &pending, Some(&worktree), reason).await;
        return Err(error.into());
    }
    spawn(services, pending, worktree, prompt.text).await
}

/// What [`follow_up_run`] needs.
pub(crate) struct RunFollowupRequest {
    pub run_id: RunId,
    pub reply: String,
    pub model: String,
    pub effort: String,
    pub capabilities: ClaudeCapabilities,
    pub rules_path: PathBuf,
}

/// Starts a follow-up on the proposal of a run without a comment (the
/// resolution of a pull before push): resumes its Claude session in its
/// worktree with the reply, as a new run (`kind = followup`, no comment).
/// The reply is kept in the run's prompt; there is no thread to add it to.
pub(crate) async fn follow_up_run(
    services: Arc<Services>,
    request: RunFollowupRequest,
) -> Result<RunId, RunError> {
    let RunFollowupRequest {
        run_id,
        reply,
        model,
        effort,
        capabilities,
        rules_path,
    } = request;
    let previous = services
        .store
        .runs()
        .get(run_id)
        .await?
        .ok_or(crate::proposal::ProposalError::RunNotFound(run_id))?;
    if !matches!(previous.status, RunStatus::Succeeded | RunStatus::Stopped)
        || !previous.worktree_path.is_dir()
    {
        return Err(RunError::RunNotReviewable(run_id));
    }
    let (repo, _) = crate::resolve::rebase_target(&services, &previous.worktree_path)
        .ok_or(RunError::ResolveGone)?;
    if crate::resolve::busy_worktree(&services, &previous.worktree_path).await? {
        return Err(RunError::RunBusy);
    }
    let start_sha = Sha::parse(&previous.start_sha).ok_or_else(|| GitError::Parse {
        command: "runs.start_sha".to_owned(),
        message: format!("not an object id: `{}`", previous.start_sha),
    })?;
    let session = Uuid::parse_str(previous.session_id.trim()).map_err(|_| {
        RunError::Spawn(format!(
            "the run's Claude session is not a valid id: `{}`",
            previous.session_id
        ))
    })?;
    let worktree = services
        .worktrees
        .worktree_at(&repo, previous.worktree_path.clone())?;
    let prompt = crate::prompt::resolve_followup_prompt(&reply);
    let pending = PendingRun {
        run_id: RunId::new(),
        comment_id: None,
        kind: RunKind::Followup,
        start_sha,
        model,
        effort,
        session: SessionMode::Resume(session),
        capabilities,
        rules_path,
    };
    record_run(&services, &pending, previous.worktree_path.clone(), &prompt).await?;
    spawn(services, pending, worktree, prompt.text).await
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

/// A run being started: everything [`spawn`] needs besides its worktree.
pub(crate) struct PendingRun {
    pub run_id: RunId,
    /// `None` for a run on no comment (the resolve run of a pull before
    /// push, and its follow-ups).
    pub comment_id: Option<CommentId>,
    pub kind: RunKind,
    pub start_sha: Sha,
    pub model: String,
    pub effort: String,
    pub session: SessionMode,
    pub capabilities: ClaudeCapabilities,
    pub rules_path: PathBuf,
}

impl PendingRun {
    fn session_id(&self) -> Uuid {
        match self.session {
            SessionMode::New(id) | SessionMode::Resume(id) => id,
        }
    }
}

/// Records the run row (`Queued`).
pub(crate) async fn record_run(
    services: &Services,
    run: &PendingRun,
    worktree_path: PathBuf,
    prompt: &Prompt,
) -> Result<(), RunError> {
    services
        .store
        .runs()
        .create_with_id(
            run.run_id,
            NewRun {
                comment_id: run.comment_id,
                kind: run.kind,
                session_id: run.session_id().hyphenated().to_string(),
                model: run.model.clone(),
                effort: run.effort.clone(),
                worktree_path,
                start_sha: run.start_sha.to_hex(),
                prompt: prompt.text.clone(),
            },
        )
        .await?;
    Ok(())
}

/// Moves the comment to `Running` before any expensive work, so a second
/// start of the same comment is refused by the compare-and-swap. On a
/// refusal the unstarted run row is deleted.
pub(crate) async fn claim_comment(
    services: &Services,
    run: &PendingRun,
    event: CommentEvent,
) -> Result<(), RunError> {
    let Some(comment_id) = run.comment_id else {
        return Ok(());
    };
    let store = &services.store;
    if let Err(error) = apply_event(&store.comments(), comment_id, event).await {
        if let Err(delete_error) = store.runs().delete(run.run_id).await {
            tracing::warn!(run = %run.run_id, error = %delete_error, "cannot delete unstarted run");
        }
        return Err(error.into());
    }
    Ok(())
}

/// Records a run that failed before its process ran, and moves its comment
/// on (a follow-up's comment back to `Proposed` when `worktree` still holds
/// a change).
pub(crate) async fn fail_unstarted(
    services: &Services,
    run: &PendingRun,
    worktree: Option<&Worktree>,
    reason: String,
) {
    let mut outcome = RunOutcome::new(RunStatus::Failed);
    outcome.error = Some(reason);
    let comment_event = match worktree {
        Some(worktree) if run.kind == RunKind::Followup => {
            followup_event(services, worktree, &run.start_sha, RunStatus::Failed).await
        }
        // The comment's earlier proposal is still there.
        _ if run.kind == RunKind::Resolve => Some(CommentEvent::RunSucceeded),
        _ => CommentEvent::from_run_status(RunStatus::Failed),
    };
    record_finish(
        services,
        Finish {
            run_id: run.run_id,
            comment_id: run.comment_id,
            kind: run.kind,
            comment_event,
            session_not_found: false,
        },
        outcome,
    )
    .await;
}

/// Spawns `claude` for `run` in `worktree` and hands it to [`supervise`].
pub(crate) async fn spawn(
    services: Arc<Services>,
    run: PendingRun,
    worktree: Worktree,
    prompt: String,
) -> Result<RunId, RunError> {
    let run_id = run.run_id;
    let comment_id = run.comment_id;
    let effort_arg = (!run.effort.trim().is_empty()).then_some(run.effort.as_str());
    let command = ClaudeCommand::review_run(
        worktree.path(),
        prompt,
        run.model.clone(),
        effort_arg,
        run.session,
        run.rules_path.clone(),
    )
    .adapt_to(&run.capabilities);
    let program = run.capabilities.path().to_owned();
    let mut child = match command.to_command(&program).spawn() {
        Ok(child) => child,
        Err(error) => {
            let reason = format!("cannot start {}: {error}", program.display());
            fail_unstarted(&services, &run, Some(&worktree), reason.clone()).await;
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

    // Running; register for Stop before anyone can see the run.
    let store = &services.store;
    if let Err(error) = store.runs().set_status(run_id, RunStatus::Running).await {
        tracing::warn!(run = %run_id, %error, "cannot mark the run as running");
    }
    let (stop_tx, stop_rx) = watch::channel(false);
    services
        .lock_active()
        .insert(run_id, ActiveRun { stop: stop_tx });
    tracing::info!(
        run = %run_id,
        comment = ?comment_id,
        kind = %run.kind,
        session = ?run.session,
        worktree = %worktree.path().display(),
        start_sha = %run.start_sha,
        "started run"
    );
    services.publish(RunUpdate::Started { run_id, comment_id });
    tokio::spawn(supervise(
        services.clone(),
        LiveRun {
            run_id,
            comment_id,
            kind: run.kind,
            resumed: matches!(run.session, SessionMode::Resume(_)),
            worktree,
            start_sha: run.start_sha,
        },
        child,
        stop_rx,
    ));
    Ok(run_id)
}

/// A spawned run, as its supervisor sees it.
struct LiveRun {
    run_id: RunId,
    comment_id: Option<CommentId>,
    kind: RunKind,
    /// Started with `--resume`.
    resumed: bool,
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

    let (outcome, session_not_found) = match exit {
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
            (outcome, false)
        }
    };
    let comment_event = match run.kind {
        RunKind::Followup => {
            followup_event(&services, &run.worktree, &run.start_sha, outcome.status).await
        }
        // However the resolution went, the comment's earlier proposal (or
        // this one) is there to review.
        RunKind::Resolve => Some(CommentEvent::RunSucceeded),
        RunKind::Comment => CommentEvent::from_run_status(outcome.status),
    };
    if run.kind == RunKind::Resolve
        && !matches!(outcome.status, RunStatus::Succeeded | RunStatus::Stopped)
    {
        // A resolution with nothing to review: its conflicted worktree is
        // of no use, and a pull before push it belonged to is abandoned.
        crate::resolve::discard_failed(&services, &run.worktree).await;
    }
    record_finish(
        &services,
        Finish {
            run_id,
            comment_id: run.comment_id,
            kind: run.kind,
            comment_event,
            session_not_found,
        },
        outcome,
    )
    .await;
}

/// The comment event of a finished follow-up. Its worktree holds the change
/// of the earlier runs too, so whatever this run did, the comment stays
/// `Proposed` while `start_sha → worktree` is non-empty.
async fn followup_event(
    services: &Services,
    worktree: &Worktree,
    start_sha: &Sha,
    status: RunStatus,
) -> Option<CommentEvent> {
    if status == RunStatus::Succeeded {
        return Some(CommentEvent::RunSucceeded);
    }
    match services
        .worktrees
        .diff_against_start(worktree, start_sha)
        .await
    {
        Ok(patch) if !patch.is_empty() => Some(CommentEvent::RunSucceeded),
        Ok(_) => CommentEvent::from_run_status(status),
        Err(error) => {
            tracing::warn!(%error, "cannot check the follow-up's worktree");
            CommentEvent::from_run_status(status)
        }
    }
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

/// The terminal outcome of an exited run, and whether its `--resume`
/// failed because the session is gone.
async fn complete(
    services: &Services,
    run: &LiveRun,
    exit: ExitStatus,
    stopped: bool,
    state: &StreamState,
    stderr_tail: &str,
    wall_ms: i64,
) -> (RunOutcome, bool) {
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
        return (outcome, false);
    }
    if !exit.success() || result.is_some_and(|r| r.is_error) {
        let reason = failure_reason(exit, result, stderr_tail);
        let missing = run.resumed
            && (reports_missing_session(stderr_tail)
                || result
                    .and_then(|r| r.result.as_deref())
                    .is_some_and(reports_missing_session));
        outcome.error = Some(if missing {
            // The fixed prefix lets the UI recognize the failure later.
            format!(
                "{SESSION_NOT_FOUND} (session not found). Run fresh with the thread as context \
                 to start a new session.\n\n{reason}"
            )
        } else {
            reason
        });
        return (outcome, missing);
    }
    // A pull-before-push resolution may rightly leave the stopped commit
    // empty (the upstream already has its change): still a proposal.
    let empty_is_proposal = run.kind == RunKind::Resolve && run.comment_id.is_none();
    match services
        .worktrees
        .diff_against_start(&run.worktree, &run.start_sha)
        .await
    {
        Ok(patch) if patch.is_empty() && empty_is_proposal => {
            outcome.status = RunStatus::Succeeded;
        }
        Ok(patch) if patch.is_empty() => outcome.status = RunStatus::NoChanges,
        Ok(patch) => {
            tracing::info!(run = %run.run_id, files = patch.files.len(), "run proposed a change");
            outcome.status = RunStatus::Succeeded;
        }
        Err(error) => {
            outcome.error = Some(format!("cannot compute the proposed change: {error}"));
        }
    }
    (outcome, false)
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

/// How a run's end is recorded.
struct Finish {
    run_id: RunId,
    comment_id: Option<CommentId>,
    kind: RunKind,
    /// What happens to the comment.
    comment_event: Option<CommentEvent>,
    session_not_found: bool,
}

/// Writes the outcome, moves the comment on, adds Claude's final message to
/// the thread, and publishes the result.
async fn record_finish(services: &Services, finish: Finish, outcome: RunOutcome) {
    let Finish {
        run_id,
        comment_id,
        kind,
        comment_event,
        session_not_found,
    } = finish;
    let status = outcome.status;
    let finished = RunFinished {
        run_id,
        comment_id,
        status,
        summary: outcome.summary.clone(),
        error: outcome.error.clone(),
        kind,
        session_not_found,
    };
    // `finish` is a storage job, so every queued event is written first.
    if let Err(error) = services.store.runs().finish(run_id, outcome).await {
        tracing::error!(run = %run_id, %error, "cannot store the run outcome");
    }
    if let Some(comment_id) = comment_id {
        let comments = services.store.comments();
        // Claude's final message joins the thread first, so the thread reads
        // as a conversation by the time the comment's status changes.
        if matches!(status, RunStatus::Succeeded | RunStatus::NoChanges)
            && let Some(summary) = finished.summary.clone()
            && let Err(error) = comments.add_run_reply(comment_id, run_id, summary).await
        {
            tracing::warn!(run = %run_id, %error, "cannot add Claude's reply");
        }
        if let Some(event) = comment_event
            && let Err(error) = apply_event(&comments, comment_id, event).await
        {
            tracing::warn!(run = %run_id, comment = %comment_id, %error, "cannot update the comment status");
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
