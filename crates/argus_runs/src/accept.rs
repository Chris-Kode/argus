//! Accept: a proposal becomes one local commit on the head branch (spec
//! §5.3, §6.4, §11).
//!
//! Runs are parallel, accepts are not: every repository has one accept
//! queue, a tokio task fed by an mpsc channel, which handles one Accept at a
//! time, in order. Push ([`crate::push`]) goes through the same queue, so a
//! pull-before-push rebase and an Accept never move the head branch at the
//! same time. [`RunManager::accept`](crate::RunManager::accept) queues
//! a run and hands back an [`AcceptTicket`]: a stream of [`AcceptEvent`]s
//! (progress, and the two questions only the user can answer) and the final
//! [`AcceptOutcome`]. The UI answers through the `reply` channel of each
//! question; dropping the ticket cancels the Accept.
//!
//! One Accept, step by step:
//!
//! 1. Before the first Accept on a repository whose head branch is checked
//!    out in one of the user's checkouts, [`AcceptEvent::CheckedOut`] asks
//!    to continue; the answer is remembered per repository. That checkout is
//!    never touched.
//! 2. The proposal patch: `git add -A` + `git diff --cached --binary
//!    <start_sha>` in the run's worktree. The proposal of a resolve run
//!    ([`crate::resolve`]) must hold no conflict marker (`git diff --check`
//!    and a scan of its files); otherwise the Accept fails with
//!    [`AcceptError::MarkersRemain`].
//! 3. The `_integration` worktree is reset to the head tip, and the patch
//!    applied with `git apply --3way --index`. A conflict resets it again
//!    and ends the Accept with [`AcceptOutcome::Conflict`]; nothing else
//!    changes.
//! 4. Haiku writes the commit subject and body from `git diff --cached`
//!    (spec §6.4), bounded by the commit-message timeout setting; on a
//!    failure or a timeout the subject is generic and the body empty.
//! 5. [`AcceptEvent::CommitMessage`] shows the prefilled subject and body.
//!    Cancel leaves the proposal as it is.
//! 6. `git commit -F -` in the integration worktree, with the user's
//!    identity, signing config and hooks. A failing hook re-asks step 5
//!    with its output.
//! 7. `git update-ref refs/heads/<head> <new> <old>`: a compare-and-swap.
//!    When the branch moved meanwhile, steps 3–7 run once more on the new
//!    tip with the message already chosen, then the Accept fails.
//! 8. The accept is recorded, the comment becomes `Resolved`, the run's
//!    worktree is deleted (with the comment's other proposal worktrees,
//!    e.g. the original proposal of an accepted resolution) and
//!    [`RunUpdate::Accepted`] is published, so the views refresh the diff,
//!    re-anchor comments and update the Push counts.
//!
//! The user's checkout (files and index) is never read or written, and
//! nothing is pushed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use argus_diff::review::{Comment, CommentEvent, ReviewError, apply_event};
use argus_git::write::{self, RefUpdate};
use argus_git::{
    ApplyOutcome, ChangeKind, CommitOutcome, ConflictMarker, GitError, Repo, RepoId, Sha, Worktree,
    read,
};
use argus_store::{CommentId, CommentStatus, RunId, RunStatus, StoreError};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot, watch};

use crate::claude::{ClaudeCommand, ClaudeStatus};
use crate::proposal::{ProposalError, load_parts};
use crate::run::{RunUpdate, Services};
use crate::settings::ClaudeSettings;

/// Commit subject when Haiku wrote none.
pub const FALLBACK_SUBJECT: &str = "Apply review comment";

/// Settings key prefix remembering, per repository, that the user agreed to
/// move a head branch checked out in their own checkout.
pub const CHECKED_OUT_ACK_KEY: &str = "accept.checked_out_ack.";

/// Largest staged diff sent to Haiku, in bytes; a longer one is cut.
const COMMIT_DIFF_MAX_BYTES: usize = 256 * 1024;

/// How much of a failed Haiku call's stderr is logged, in bytes.
const HAIKU_ERROR_BYTES: usize = 512;

/// Reflog message of the head branch update.
const REFLOG_REASON: &str = "argus: accept";

/// Why an Accept could not complete. A conflict is not an error: it is
/// [`AcceptOutcome::Conflict`].
#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    /// The run or its proposal cannot be loaded.
    #[error(transparent)]
    Proposal(#[from] ProposalError),
    /// The run is still running.
    #[error("run {0} is still running; stop it or wait for it to finish before accepting")]
    StillRunning(RunId),
    /// The run is already queued for Accept.
    #[error("this proposal is already being accepted")]
    AlreadyQueued(RunId),
    /// The comment is resolved or dismissed.
    #[error("the comment is {0}; there is nothing to accept")]
    CommentClosed(CommentStatus),
    /// The run changed nothing.
    #[error("the proposal is empty: the run changed nothing")]
    EmptyProposal,
    /// The change is already on the head branch.
    #[error("the head branch already contains this change; there is nothing to commit")]
    AlreadyApplied,
    /// The head branch kept moving: the compare-and-swap failed twice.
    #[error(
        "`{head}` moved while Argus was committing, twice in a row; the commit was not added. \
         Try Accept again."
    )]
    BranchMoved {
        /// The head branch.
        head: String,
    },
    /// The head branch was deleted.
    #[error("the head branch `{0}` no longer exists")]
    BranchGone(String),
    /// A git operation failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The comment status could not change.
    #[error(transparent)]
    Review(#[from] ReviewError),
    /// The accept queue of the repository stopped.
    #[error("the accept queue stopped")]
    QueueClosed,
    /// The conflict resolution still has conflict markers; nothing was
    /// committed.
    #[error("{}", markers_message(.0))]
    MarkersRemain(Vec<ConflictMarker>),
}

/// How many marker locations an error names before "and N more".
const MARKERS_LISTED: usize = 5;

/// "Conflict markers remain in a.rs:3, b.rs:10 …; resolve them first".
pub(crate) fn markers_message(markers: &[ConflictMarker]) -> String {
    let mut listed: Vec<String> = markers
        .iter()
        .take(MARKERS_LISTED)
        .map(ToString::to_string)
        .collect();
    if markers.len() > MARKERS_LISTED {
        listed.push(format!("and {} more", markers.len() - MARKERS_LISTED));
    }
    format!(
        "Accept is blocked: conflict markers remain at {}. Reply to Claude or edit the files, \
         then accept again.",
        listed.join(", ")
    )
}

/// Where an Accept is, for the "Accepting…" indicator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptStage {
    /// Waiting for earlier accepts of the repository.
    Queued,
    /// Applying the patch on the head tip.
    Applying,
    /// Haiku is writing the commit body.
    WritingMessage,
    /// Waiting for the user in the commit dialog.
    AwaitingMessage,
    /// `git commit` (and its hooks) is running.
    Committing,
    /// Moving the head branch.
    MovingBranch,
    /// The branch moved underneath; applying again on the new tip.
    Retrying,
    /// Recording the accept and cleaning up.
    Finishing,
}

impl AcceptStage {
    /// Short label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Applying => "Applying the patch",
            Self::WritingMessage => "Writing the commit message",
            Self::AwaitingMessage => "Waiting for the commit message",
            Self::Committing => "Committing",
            Self::MovingBranch => "Moving the branch",
            Self::Retrying => "The branch moved; applying again",
            Self::Finishing => "Finishing",
        }
    }
}

/// The commit dialog's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDraft {
    /// The comment being resolved.
    pub comment_id: CommentId,
    /// The head branch the commit lands on.
    pub head: String,
    /// Subject line: Haiku's, [`FALLBACK_SUBJECT`] when it failed, or the
    /// user's text when re-asked.
    pub subject: String,
    /// Body: Haiku's text, empty when it failed, or the user's text when
    /// re-asked.
    pub body: String,
    /// Why the body is empty (Haiku failed or timed out), when it is.
    pub body_note: Option<String>,
    /// The output of the last `git commit` attempt, when it failed (e.g. a
    /// pre-commit hook).
    pub commit_error: Option<String>,
    /// Files in the change.
    pub files: Vec<PathBuf>,
}

impl CommitDraft {
    /// The full commit message for `subject` and `body`.
    pub fn message(&self, subject: &str, body: &str) -> String {
        compose_message(subject, body)
    }
}

/// The user's commit message, from the commit dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInput {
    /// Subject line.
    pub subject: String,
    /// Body, possibly empty.
    pub body: String,
}

/// What an Accept tells, or asks, the UI.
#[derive(Debug)]
pub enum AcceptEvent {
    /// Progress.
    Stage(AcceptStage),
    /// The head branch is checked out in one of the user's checkouts: ask
    /// once per repository whether to move it anyway. Reply `true` to
    /// continue; `false` (or dropping the sender) cancels the Accept.
    CheckedOut {
        /// The head branch.
        head: String,
        /// The checkout that has it checked out.
        path: PathBuf,
        /// The answer.
        reply: oneshot::Sender<bool>,
    },
    /// Show the commit dialog. Reply with the message, or `None` (or drop
    /// the sender) to cancel; the proposal then stays as it is.
    CommitMessage {
        /// The dialog's content.
        draft: CommitDraft,
        /// The answer.
        reply: oneshot::Sender<Option<CommitInput>>,
    },
}

/// How an Accept ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// The commit is on the head branch.
    Committed(Accepted),
    /// The patch conflicts with the head tip; nothing was committed and the
    /// proposal stays as it is (spec §6.6).
    Conflict(AcceptConflict),
    /// The user cancelled; the proposal stays as it is.
    Cancelled,
}

/// A landed Accept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// The run whose proposal was accepted.
    pub run_id: RunId,
    /// The resolved comment.
    pub comment_id: CommentId,
    /// The head branch.
    pub head: String,
    /// The new commit, now the head tip.
    pub commit: Sha,
    /// The full commit message.
    pub message: String,
    /// The branch moved underneath once and the patch was applied again.
    pub retried: bool,
    /// Steps after the commit landed that failed (e.g. the run worktree
    /// could not be deleted); the commit itself is fine.
    pub warnings: Vec<String>,
}

/// An Accept that hit a conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptConflict {
    /// The run whose proposal conflicts.
    pub run_id: RunId,
    /// Its comment.
    pub comment_id: CommentId,
    /// The comment's first line, to name it in the conflict dialog.
    pub comment_label: String,
    /// The head branch.
    pub head: String,
    /// The head tip the patch was applied on.
    pub head_tip: Sha,
    /// The conflicted files, relative to the repository root.
    pub files: Vec<PathBuf>,
    /// What `git apply` printed.
    pub detail: String,
}

/// A queued Accept: its events and its result. Dropping it cancels the
/// Accept at its next question (or immediately, while Haiku writes).
#[derive(Debug)]
pub struct AcceptTicket {
    /// The run being accepted.
    pub run_id: RunId,
    /// Progress and questions, in order.
    pub events: mpsc::UnboundedReceiver<AcceptEvent>,
    /// The outcome, sent once.
    pub result: oneshot::Receiver<Result<AcceptOutcome, AcceptError>>,
}

/// What the accept queues need besides the run services.
#[derive(Clone, Debug)]
pub(crate) struct AcceptEnv {
    pub services: Arc<Services>,
    pub settings: watch::Receiver<ClaudeSettings>,
    pub claude: watch::Receiver<ClaudeStatus>,
}

/// One Accept waiting in a queue.
pub(crate) struct Job {
    run_id: RunId,
    events: mpsc::UnboundedSender<AcceptEvent>,
    result: oneshot::Sender<Result<AcceptOutcome, AcceptError>>,
}

/// Work for a repository's queue: an Accept or a Push, handled one at a
/// time in submission order.
pub(crate) enum QueueJob {
    /// Commit a proposal.
    Accept(Job),
    /// Pull with rebase if needed, then push.
    Push(Box<crate::push::PushJob>),
}

type Queues = HashMap<RepoId, mpsc::UnboundedSender<QueueJob>>;

/// The per-repository accept queues.
#[derive(Debug, Default)]
pub(crate) struct AcceptQueues {
    queues: Mutex<Queues>,
    pending: Arc<Mutex<HashSet<RunId>>>,
}

impl AcceptQueues {
    fn lock_queues(&self) -> MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hands `job` to the repository's queue, starting the queue task if it
    /// is not running. Must run on the tokio runtime. Gives the job back
    /// when the queue cannot take it (runtime shutdown).
    pub fn enqueue(&self, env: &AcceptEnv, repo: &RepoId, job: QueueJob) -> Result<(), QueueJob> {
        let mut job = job;
        let mut queues = self.lock_queues();
        // A queue whose task ended (runtime shutdown) is replaced once.
        for _ in 0..2 {
            let sender = queues.entry(repo.clone()).or_insert_with(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(queue_loop(env.clone(), rx, self.pending.clone()));
                tracing::debug!(%repo, "started accept queue");
                tx
            });
            match sender.send(job) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::SendError(returned)) => {
                    job = returned;
                    queues.remove(repo);
                }
            }
        }
        Err(job)
    }

    /// Runs queued or being accepted.
    pub fn pending(&self) -> Vec<RunId> {
        lock(&self.pending).iter().copied().collect()
    }

    /// Queues the Accept of `run_id` behind the repository's earlier ones.
    /// Must run on the tokio runtime.
    pub async fn submit(
        &self,
        env: &AcceptEnv,
        run_id: RunId,
    ) -> Result<AcceptTicket, AcceptError> {
        if env.services.is_active(run_id) {
            return Err(AcceptError::StillRunning(run_id));
        }
        let (run, comment, repo, _) = load_parts(&env.services, run_id).await?;
        check_comment(&comment)?;
        drop(run);
        if !lock(&self.pending).insert(run_id) {
            return Err(AcceptError::AlreadyQueued(run_id));
        }
        let (events_tx, events) = mpsc::unbounded_channel();
        let (result_tx, result) = oneshot::channel();
        let _ = events_tx.send(AcceptEvent::Stage(AcceptStage::Queued));
        let job = Job {
            run_id,
            events: events_tx,
            result: result_tx,
        };
        match self.enqueue(env, repo.id(), QueueJob::Accept(job)) {
            Ok(()) => {
                tracing::info!(run = %run_id, repo = %repo.id(), "accept queued");
                Ok(AcceptTicket {
                    run_id,
                    events,
                    result,
                })
            }
            Err(_) => {
                lock(&self.pending).remove(&run_id);
                Err(AcceptError::QueueClosed)
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A comment that can still be resolved: `Proposed`, or `Open` with the
/// partial change of a stopped run.
fn check_comment(comment: &Comment) -> Result<(), AcceptError> {
    match comment.status {
        CommentStatus::Proposed | CommentStatus::Open => Ok(()),
        status => Err(AcceptError::CommentClosed(status)),
    }
}

/// One repository's queue: one Accept or Push at a time, in submission
/// order.
async fn queue_loop(
    env: AcceptEnv,
    mut jobs: mpsc::UnboundedReceiver<QueueJob>,
    pending: Arc<Mutex<HashSet<RunId>>>,
) {
    while let Some(job) = jobs.recv().await {
        let job = match job {
            QueueJob::Accept(job) => job,
            QueueJob::Push(job) => {
                crate::push::run_job(&env, *job).await;
                continue;
            }
        };
        let run_id = job.run_id;
        let result = if job.events.is_closed() {
            // Cancelled while queued.
            Ok(AcceptOutcome::Cancelled)
        } else {
            accept(&env, run_id, &job.events).await
        };
        match &result {
            Ok(AcceptOutcome::Committed(accepted)) => {
                tracing::info!(run = %run_id, commit = %accepted.commit, "accepted");
            }
            Ok(AcceptOutcome::Conflict(conflict)) => {
                tracing::info!(run = %run_id, files = conflict.files.len(), "accept conflicts");
            }
            Ok(AcceptOutcome::Cancelled) => tracing::info!(run = %run_id, "accept cancelled"),
            Err(error) => tracing::warn!(run = %run_id, %error, "accept failed"),
        }
        lock(&pending).remove(&run_id);
        let _ = job.result.send(result);
    }
}

/// Sends an event; `false` when the UI dropped the ticket.
fn emit(events: &mpsc::UnboundedSender<AcceptEvent>, event: AcceptEvent) -> bool {
    events.send(event).is_ok()
}

fn stage(events: &mpsc::UnboundedSender<AcceptEvent>, stage: AcceptStage) {
    let _ = events.send(AcceptEvent::Stage(stage));
}

/// One Accept, from the patch to the moved branch (see the module docs).
async fn accept(
    env: &AcceptEnv,
    run_id: RunId,
    events: &mpsc::UnboundedSender<AcceptEvent>,
) -> Result<AcceptOutcome, AcceptError> {
    let services = &env.services;
    if services.is_active(run_id) {
        return Err(AcceptError::StillRunning(run_id));
    }
    let (run, comment, repo, run_worktree) = load_parts(services, run_id).await?;
    check_comment(&comment)?;
    if !run_worktree.path().is_dir() {
        return Err(ProposalError::WorktreeGone(run_id).into());
    }
    let start_sha = Sha::parse(&run.start_sha).ok_or_else(|| GitError::Parse {
        command: "runs.start_sha".to_owned(),
        message: format!("not an object id: `{}`", run.start_sha),
    })?;
    let head = comment.head_ref.clone();

    // 1. The head branch checked out in the user's checkout (spec Q1).
    if !confirm_checked_out(env, &repo, &comment, events).await? {
        return Ok(AcceptOutcome::Cancelled);
    }

    // 2. The proposal patch.
    stage(events, AcceptStage::Applying);
    let patch = services
        .worktrees
        .diff_against_start(&run_worktree, &start_sha)
        .await?;
    if patch.is_empty() {
        return Err(AcceptError::EmptyProposal);
    }
    let files: Vec<PathBuf> = patch.files.iter().map(|file| file.path.clone()).collect();
    if crate::resolve::resolution_of(services, &run)
        .await?
        .is_some()
    {
        let scan: Vec<PathBuf> = patch
            .files
            .iter()
            .filter(|file| file.kind != ChangeKind::Deleted)
            .map(|file| file.path.clone())
            .collect();
        let markers = services
            .worktrees
            .conflict_markers(&run_worktree, &start_sha, &scan)
            .await?;
        if !markers.is_empty() {
            return Err(AcceptError::MarkersRemain(markers));
        }
    }

    let mut chosen: Option<CommitInput> = None;
    let mut retried = false;
    loop {
        // 3. Apply on the head tip in the integration worktree.
        let old = match write::branch_tip(&repo, &head).await {
            Ok(tip) => tip,
            Err(GitError::RefNotFound { .. }) => return Err(AcceptError::BranchGone(head)),
            Err(error) => return Err(error.into()),
        };
        let integration = services.worktrees.integration_worktree(&repo, &old).await?;
        if let ApplyOutcome::Conflict { files, detail } = services
            .worktrees
            .apply_patch(&integration, &patch.bytes)
            .await?
        {
            return Ok(AcceptOutcome::Conflict(AcceptConflict {
                run_id,
                comment_id: comment.id,
                comment_label: comment
                    .body_md
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("review comment")
                    .to_owned(),
                head,
                head_tip: old,
                files,
                detail,
            }));
        }
        let staged = services.worktrees.staged_diff(&integration).await?;
        if staged.is_empty() {
            services.worktrees.reset(&integration, &old).await?;
            return Err(AcceptError::AlreadyApplied);
        }

        // 4–6. The message and the commit.
        let draft = match &chosen {
            // The retry after a lost compare-and-swap keeps the message.
            Some(input) => {
                let mut draft = base_draft(&comment, &head, files.clone());
                draft.subject = input.subject.clone();
                draft.body = input.body.clone();
                draft
            }
            None => {
                stage(events, AcceptStage::WritingMessage);
                let text = tokio::select! {
                    text = commit_text(env, integration.path(), staged, &comment.body_md) => text,
                    () = events.closed() => {
                        services.worktrees.reset(&integration, &old).await?;
                        return Ok(AcceptOutcome::Cancelled);
                    }
                };
                let mut draft = base_draft(&comment, &head, files.clone());
                match text {
                    Ok((subject, body)) => {
                        if !subject.is_empty() {
                            draft.subject = subject;
                        }
                        draft.body = body;
                    }
                    Err(note) => draft.body_note = Some(note),
                }
                draft
            }
        };
        let ask_first = chosen.is_none();
        let Some((commit, input)) =
            commit_with_dialog(env, &integration, draft, ask_first, events).await?
        else {
            services.worktrees.reset(&integration, &old).await?;
            return Ok(AcceptOutcome::Cancelled);
        };
        let message = compose_message(&input.subject, &input.body);

        // 7. Compare-and-swap the head branch.
        stage(events, AcceptStage::MovingBranch);
        match write::update_branch(&repo, &head, &commit, &old, REFLOG_REASON).await? {
            RefUpdate::Moved => {
                stage(events, AcceptStage::Finishing);
                let warnings =
                    finish(env, run_id, &comment, &run_worktree, &commit, &message).await;
                return Ok(AcceptOutcome::Committed(Accepted {
                    run_id,
                    comment_id: comment.id,
                    head,
                    commit,
                    message,
                    retried,
                    warnings,
                }));
            }
            RefUpdate::Stale { current: None } => return Err(AcceptError::BranchGone(head)),
            RefUpdate::Stale {
                current: Some(current),
            } => {
                if retried {
                    return Err(AcceptError::BranchMoved { head });
                }
                tracing::info!(run = %run_id, expected = %old, %current, "head moved; retrying the accept");
                retried = true;
                chosen = Some(input);
                stage(events, AcceptStage::Retrying);
            }
        }
    }
}

/// Asks for the message (unless `ask_first` is false, on a retry) and
/// commits; a failed commit re-asks with git's output. `None` when the user
/// cancelled.
async fn commit_with_dialog(
    env: &AcceptEnv,
    integration: &Worktree,
    mut draft: CommitDraft,
    ask_first: bool,
    events: &mpsc::UnboundedSender<AcceptEvent>,
) -> Result<Option<(Sha, CommitInput)>, AcceptError> {
    let mut ask = ask_first;
    loop {
        let input = if ask {
            stage(events, AcceptStage::AwaitingMessage);
            let (reply, answer) = oneshot::channel();
            if !emit(
                events,
                AcceptEvent::CommitMessage {
                    draft: draft.clone(),
                    reply,
                },
            ) {
                return Ok(None);
            }
            match answer.await {
                Ok(Some(input)) => input,
                Ok(None) | Err(_) => return Ok(None),
            }
        } else {
            CommitInput {
                subject: draft.subject.clone(),
                body: draft.body.clone(),
            }
        };
        stage(events, AcceptStage::Committing);
        let message = draft.message(&input.subject, &input.body);
        match env.services.worktrees.commit(integration, &message).await? {
            CommitOutcome::Committed(sha) => return Ok(Some((sha, input))),
            CommitOutcome::Failed { output } => {
                tracing::info!(%output, "git commit failed; asking again");
                draft.subject = input.subject;
                draft.body = input.body;
                draft.commit_error = Some(if output.is_empty() {
                    "git commit failed without output".to_owned()
                } else {
                    output
                });
                ask = true;
            }
        }
    }
}

/// Step 8: records the accept, resolves the comment, deletes the run's
/// worktree and publishes the update. The commit already landed, so
/// failures are returned as warnings, not errors.
async fn finish(
    env: &AcceptEnv,
    run_id: RunId,
    comment: &Comment,
    run_worktree: &Worktree,
    commit: &Sha,
    message: &str,
) -> Vec<String> {
    let services = &env.services;
    let mut warnings = Vec::new();
    if let Err(error) = services
        .store
        .accepts()
        .record(run_id, commit.to_hex(), message.to_owned())
        .await
    {
        warnings.push(format!("could not record the accept: {error}"));
    }
    let comments = services.store.comments();
    let resolved = match comment.status {
        CommentStatus::Proposed => apply_event(&comments, comment.id, CommentEvent::Accepted)
            .await
            .map(drop)
            .map_err(|error| error.to_string()),
        // A stopped run's partial change: its comment went back to `Open`,
        // and accepting it resolves the comment all the same.
        _ => comments
            .set_status_if(comment.id, comment.status, CommentStatus::Resolved)
            .await
            .map_err(|error| error.to_string())
            .and_then(|changed| {
                changed
                    .then_some(())
                    .ok_or_else(|| "its status changed meanwhile".to_owned())
            }),
    };
    if let Err(error) = resolved {
        warnings.push(format!("could not resolve the comment: {error}"));
    }
    if let Err(error) = services.worktrees.remove(run_worktree).await {
        warnings.push(format!("could not delete the run worktree: {error}"));
    }
    // The comment's other proposals (the original one of an accepted
    // resolution, or one replaced by a resolution) are moot now.
    match services.store.runs().list_for_comment(comment.id).await {
        Ok(runs) => {
            let mut removed: Vec<PathBuf> = vec![run_worktree.path().to_owned()];
            for other in runs {
                if other.id == run_id
                    || !matches!(other.status, RunStatus::Succeeded | RunStatus::Stopped)
                    || removed.contains(&other.worktree_path)
                    || !other.worktree_path.is_dir()
                {
                    continue;
                }
                removed.push(other.worktree_path.clone());
                let worktree = services
                    .worktrees
                    .worktree_at_dir(run_worktree.repo_dir(), other.worktree_path.clone());
                if let Err(error) = match worktree {
                    Ok(worktree) => services.worktrees.remove(&worktree).await,
                    Err(error) => Err(error),
                } {
                    warnings.push(format!(
                        "could not delete the earlier proposal worktree: {error}"
                    ));
                }
            }
        }
        Err(error) => warnings.push(format!("could not list the comment's runs: {error}")),
    }
    for warning in &warnings {
        tracing::warn!(run = %run_id, %warning, "accept cleanup");
    }
    services.publish(RunUpdate::Accepted {
        run_id,
        comment_id: Some(comment.id),
        commit: *commit,
    });
    warnings
}

/// Asks before the first Accept on a repository whose head branch is
/// checked out in a checkout of the user's (not an Argus worktree). `true`
/// to go on.
async fn confirm_checked_out(
    env: &AcceptEnv,
    repo: &Repo,
    comment: &Comment,
    events: &mpsc::UnboundedSender<AcceptEvent>,
) -> Result<bool, AcceptError> {
    let services = &env.services;
    if checked_out_acknowledged(services, comment.repo_id).await? {
        return Ok(true);
    }
    let Some(path) = user_checkout_of(services, repo, &comment.head_ref).await? else {
        return Ok(true);
    };
    let (reply, answer) = oneshot::channel();
    if !emit(
        events,
        AcceptEvent::CheckedOut {
            head: comment.head_ref.clone(),
            path,
            reply,
        },
    ) {
        return Ok(false);
    }
    if answer.await != Ok(true) {
        return Ok(false);
    }
    acknowledge_checked_out(services, comment.repo_id).await?;
    Ok(true)
}

/// Whether the user already agreed, for this repository, that Argus moves
/// a head branch checked out in one of their checkouts.
pub(crate) async fn checked_out_acknowledged(
    services: &Services,
    repo_id: argus_store::RepoId,
) -> Result<bool, StoreError> {
    let key = format!("{CHECKED_OUT_ACK_KEY}{repo_id}");
    Ok(services.store.settings().get::<bool>(key).await? == Some(true))
}

/// Remembers that the user agreed to move checked-out head branches of this
/// repository.
pub(crate) async fn acknowledge_checked_out(
    services: &Services,
    repo_id: argus_store::RepoId,
) -> Result<(), StoreError> {
    let key = format!("{CHECKED_OUT_ACK_KEY}{repo_id}");
    services.store.settings().set(key, &true).await
}

/// The checkout of the user's (not an Argus worktree) that has `head`
/// checked out, if any.
pub(crate) async fn user_checkout_of(
    services: &Services,
    repo: &Repo,
    head: &str,
) -> Result<Option<PathBuf>, GitError> {
    let root = services.worktrees.root();
    Ok(read::current_checkouts(repo)
        .await?
        .into_iter()
        .find(|checkout| {
            checkout.branch.as_deref() == Some(head)
                && !std::fs::canonicalize(&checkout.path)
                    .unwrap_or_else(|_| checkout.path.clone())
                    .starts_with(root)
        })
        .map(|checkout| checkout.path))
}

/// The dialog's prefilled content: the fallback subject, no body.
fn base_draft(comment: &Comment, head: &str, files: Vec<PathBuf>) -> CommitDraft {
    CommitDraft {
        comment_id: comment.id,
        head: head.to_owned(),
        subject: FALLBACK_SUBJECT.to_owned(),
        body: String::new(),
        body_note: None,
        commit_error: None,
        files,
    }
}

/// Subject, blank line, body (when there is one).
pub fn compose_message(subject: &str, body: &str) -> String {
    let subject = subject.lines().map(str::trim).collect::<Vec<_>>().join(" ");
    let subject = subject.trim();
    let body = body.trim();
    let mut message = String::from(subject);
    if !body.is_empty() {
        message.push_str("\n\n");
        message.push_str(body);
    }
    message.push('\n');
    message
}

/// Haiku's commit subject and body for the staged diff (spec §6.4), within
/// the commit-message timeout. `Err` carries why the body is empty.
async fn commit_text(
    env: &AcceptEnv,
    cwd: &Path,
    mut diff: Vec<u8>,
    comment_text: &str,
) -> Result<(String, String), String> {
    let settings = env.settings.borrow().clone();
    let timeout = settings.commit_message_timeout();
    let detected = env
        .claude
        .borrow()
        .capabilities()
        .map(|capabilities| capabilities.path().to_owned());
    let program = match detected {
        Some(path) => path,
        None => tokio::task::spawn_blocking(move || settings.resolve_claude())
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("Claude Code is unavailable: {error}"))?,
    };
    if diff.len() > COMMIT_DIFF_MAX_BYTES {
        diff.truncate(COMMIT_DIFF_MAX_BYTES);
        diff.extend_from_slice(b"\n[diff truncated]\n");
    }
    let command = ClaudeCommand::commit_message(diff, comment_text).with_cwd(cwd);
    match tokio::time::timeout(timeout, run_claude(&command, &program)).await {
        Ok(Ok(text)) => {
            let (subject, body) = split_message(&text);
            if subject.is_empty() && body.is_empty() {
                Err("Haiku returned an empty message".to_owned())
            } else {
                Ok((subject, body))
            }
        }
        Ok(Err(error)) => {
            tracing::info!(%error, "commit message generation failed");
            Err(format!("Haiku failed: {error}"))
        }
        Err(_) => {
            tracing::info!(?timeout, "commit message generation timed out");
            Err(format!(
                "Haiku did not answer within {}",
                format_timeout(timeout)
            ))
        }
    }
}

/// Runs the commit-message command to completion and returns its stdout.
/// Dropping the future kills `claude`.
async fn run_claude(command: &ClaudeCommand, program: &Path) -> Result<String, String> {
    let mut process = command.to_command(program);
    process.stdin(Stdio::piped());
    let mut child = process
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", program.display()))?;
    let input = command.stdin().map(<[u8]>::to_vec).unwrap_or_default();
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let write = async move {
        if let Some(stdin) = stdin.as_mut() {
            // Claude may exit before reading the whole diff; its exit
            // status tells what went wrong.
            let _ = stdin.write_all(&input).await;
        }
        drop(stdin);
    };
    let read_out = async move {
        let mut buffer = Vec::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_end(&mut buffer).await;
        }
        buffer
    };
    let read_err = async move {
        let mut buffer = Vec::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_end(&mut buffer).await;
        }
        buffer
    };
    let ((), out, err) = tokio::join!(write, read_out, read_err);
    let status = child.wait().await.map_err(|error| error.to_string())?;
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let out = String::from_utf8_lossy(&out);
        let detail = [err.trim(), out.trim()]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("no output");
        return Err(format!("{status}: {}", prefix(detail, HAIKU_ERROR_BYTES)));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Haiku's answer as a commit subject and body: surrounding code fences
/// removed, the first line is the subject (at most 72 characters, without a
/// trailing period) and the rest the body. A first line too long for a
/// subject stays in the body and the subject is empty.
pub fn split_message(text: &str) -> (String, String) {
    let mut text = text.trim();
    if let Some(inner) = text.strip_prefix("```") {
        let inner = inner.split_once('\n').map_or("", |(_, rest)| rest);
        text = inner.trim_end().strip_suffix("```").unwrap_or(inner).trim();
    }
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let first = first.trim().trim_start_matches('#').trim();
    if first.chars().count() > 72 {
        return (String::new(), text.to_owned());
    }
    let subject = first.strip_suffix('.').unwrap_or(first).trim().to_owned();
    (subject, rest.trim().to_owned())
}

fn format_timeout(timeout: Duration) -> String {
    format!("{} s", timeout.as_secs())
}

/// The longest prefix of `text` of at most `max` bytes, on a char boundary.
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
