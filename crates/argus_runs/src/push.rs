//! Push: the head branch goes to its upstream, after a pull with rebase when
//! the remote is ahead (spec §5.4, §11).
//!
//! Commits stay local until the user clicks Push; Argus never pushes on its
//! own and never force-pushes. [`RunManager::push`](crate::RunManager::push)
//! puts a Push on the repository's accept queue (see [`crate::accept`]), so
//! its rebase and an Accept never move the head branch at the same time. It
//! hands back a [`PushTicket`]: a stream of [`PushEvent`]s (steps, each git
//! command with its output, and one question) and the final
//! [`PushOutcome`]. Dropping the ticket cancels the Push at its next step.
//!
//! One Push, step by step, every git command running in the repository's
//! `_integration` worktree (never the user's checkout):
//!
//! 1. `git fetch <remote> <branch>` of the head's upstream only, into its
//!    remote-tracking ref.
//! 2. If the remote is ahead: the integration worktree is reset to the
//!    local head tip and `git rebase <remote>/<branch>` runs there. A
//!    conflict aborts the rebase and ends the Push with
//!    [`PushOutcome::Conflict`] (operation "Pull before push"); nothing
//!    moves and nothing is pushed. On success the head branch moves to the
//!    rebased commit with a compare-and-swap `git update-ref`. Before the
//!    first such move on a repository whose head branch is checked out in a
//!    checkout of the user's, [`PushEvent::CheckedOut`] asks to continue
//!    (the answer is shared with Accept's).
//! 3. `git push <remote> refs/heads/<head>:refs/heads/<branch>`: no
//!    `--force`, no `--force-with-lease`. A rejected push (the remote moved
//!    again) runs everything once more from step 1, then fails.
//!
//! With [`PushMode::SetUpstream`] (the head has no upstream yet) the target
//! is `<remote>/<head>` and the push carries `--set-upstream`.

use std::path::PathBuf;

use argus_git::push::{self as git_push, FetchOutcome, PushResult, StepLog, Upstream};
use argus_git::write::{self, RefUpdate};
use argus_git::{GitError, RebaseOutcome, Repo, Sha};
use argus_store::StoreError;
use tokio::sync::{mpsc, oneshot};

use crate::accept::{
    AcceptEnv, AcceptQueues, QueueJob, acknowledge_checked_out, checked_out_acknowledged,
    user_checkout_of,
};

/// The operation named in the conflict dialog when the rebase conflicts.
pub const PULL_BEFORE_PUSH: &str = "Pull before push";

/// Reflog message of the head branch update after the rebase.
const REFLOG_REASON: &str = "argus: pull before push";

/// How many times the whole sequence runs when the remote keeps moving.
const ATTEMPTS: u32 = 2;

/// What to push to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushMode {
    /// The head's configured upstream.
    Upstream,
    /// The head has no upstream: push to `<remote>/<head>` and record it as
    /// the upstream (`git push --set-upstream`).
    SetUpstream {
        /// The remote picked by the user.
        remote: String,
    },
}

/// A Push to queue.
#[derive(Clone, Debug)]
pub struct PushRequest {
    /// The repository's stored id (for the checked-out acknowledgement).
    pub repo_id: argus_store::RepoId,
    /// The repository.
    pub repo: Repo,
    /// The head branch (short name, local).
    pub head: String,
    /// Where it goes.
    pub mode: PushMode,
}

/// Where a Push is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushStep {
    /// Waiting for accepts (or another Push) of the repository.
    Queued,
    /// Fetching the upstream branch.
    Fetching,
    /// Rebasing the local commits on the remote's.
    Rebasing,
    /// Moving the head branch to the rebased commit.
    MovingBranch,
    /// Pushing.
    Pushing,
    /// The remote moved (or the branch moved); starting again from the
    /// fetch.
    Retrying,
}

impl PushStep {
    /// Short label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "Waiting for accepts to finish",
            Self::Fetching => "Fetching the upstream",
            Self::Rebasing => "Rebasing on the upstream",
            Self::MovingBranch => "Moving the branch",
            Self::Pushing => "Pushing",
            Self::Retrying => "The remote moved; trying again",
        }
    }
}

/// What a Push tells, or asks, the UI.
#[derive(Debug)]
pub enum PushEvent {
    /// A new step started.
    Step(PushStep),
    /// A git command finished: its command line and output.
    Log(StepLog),
    /// The rebase will move a head branch checked out in one of the user's
    /// checkouts: ask once per repository (shared with Accept). Reply `true`
    /// to continue; `false` (or dropping the sender) cancels the Push.
    CheckedOut {
        /// The head branch.
        head: String,
        /// The checkout that has it checked out.
        path: PathBuf,
        /// The answer.
        reply: oneshot::Sender<bool>,
    },
}

/// How a Push ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// The remote branch now has the head's commits.
    Pushed(PushReport),
    /// There was nothing to push (possibly after pulling the remote's
    /// commits).
    UpToDate(PushReport),
    /// The rebase on the remote's commits conflicts; nothing moved and
    /// nothing was pushed.
    Conflict(PushConflict),
    /// The user cancelled; nothing was pushed (a rebase that already landed
    /// stays).
    Cancelled,
}

/// A successful Push (or a pull with nothing left to push).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushReport {
    /// The head branch.
    pub head: String,
    /// The upstream, `origin/feature`.
    pub upstream: String,
    /// The head tip, now on the remote (for [`PushOutcome::Pushed`]).
    pub commit: Sha,
    /// The head branch was rebased on the remote's commits: from, to.
    pub rebased: Option<(Sha, Sha)>,
    /// The remote moved during the Push and the sequence ran again.
    pub retried: bool,
    /// The upstream was recorded with `--set-upstream`.
    pub set_upstream: bool,
    /// Commits on head the upstream lacks, after the Push.
    pub ahead: u32,
    /// Commits on the upstream head lacks, after the Push.
    pub behind: u32,
}

/// A pull-before-push rebase that conflicts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushConflict {
    /// The operation, [`PULL_BEFORE_PUSH`].
    pub operation: &'static str,
    /// The head branch.
    pub head: String,
    /// The upstream, `origin/feature`.
    pub upstream: String,
    /// The local head tip the rebase started from.
    pub head_tip: Sha,
    /// The remote tip the rebase targeted.
    pub upstream_tip: Sha,
    /// The conflicted files, relative to the repository root.
    pub files: Vec<PathBuf>,
    /// What `git rebase` printed.
    pub detail: String,
}

/// Why a Push failed. A conflict is not an error: it is
/// [`PushOutcome::Conflict`].
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// The head branch has no upstream (or a local one).
    #[error("`{0}` has no upstream on a remote; push it with an upstream first")]
    NoUpstream(String),
    /// The head branch was deleted.
    #[error("the head branch `{0}` no longer exists")]
    BranchGone(String),
    /// The head branch moved while Argus rebased it, twice.
    #[error("`{head}` moved while Argus was rebasing it, twice in a row; nothing was pushed")]
    BranchMoved {
        /// The head branch.
        head: String,
    },
    /// The remote rejected the push twice: it keeps moving.
    #[error("the remote rejected the push twice (it moved again); nothing was pushed:\n{output}")]
    Rejected {
        /// git's output of the last push.
        output: String,
    },
    /// A git step failed; `output` is what git printed.
    #[error("`{command}` failed:\n{output}")]
    Step {
        /// The command line.
        command: String,
        /// git's output.
        output: String,
    },
    /// A git operation failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The repository's queue stopped.
    #[error("the accept queue stopped")]
    QueueClosed,
}

impl PushError {
    fn step(log: &StepLog) -> Self {
        Self::Step {
            command: log.command.clone(),
            output: if log.output.is_empty() {
                "(no output)".to_owned()
            } else {
                log.output.clone()
            },
        }
    }
}

/// A queued Push: its events and its result. Dropping it cancels the Push
/// at its next step.
#[derive(Debug)]
pub struct PushTicket {
    /// Steps, git output and the question, in order.
    pub events: mpsc::UnboundedReceiver<PushEvent>,
    /// The outcome, sent once.
    pub result: oneshot::Receiver<Result<PushOutcome, PushError>>,
}

/// One Push waiting in a queue.
pub(crate) struct PushJob {
    request: PushRequest,
    events: mpsc::UnboundedSender<PushEvent>,
    result: oneshot::Sender<Result<PushOutcome, PushError>>,
}

/// Queues a Push behind the repository's earlier accepts and pushes. Must
/// run on the tokio runtime.
pub(crate) fn submit(
    queues: &AcceptQueues,
    env: &AcceptEnv,
    request: PushRequest,
) -> Result<PushTicket, PushError> {
    let (events_tx, events) = mpsc::unbounded_channel();
    let (result_tx, result) = oneshot::channel();
    let _ = events_tx.send(PushEvent::Step(PushStep::Queued));
    let repo_id = request.repo.id().clone();
    let job = PushJob {
        request,
        events: events_tx,
        result: result_tx,
    };
    queues
        .enqueue(env, &repo_id, QueueJob::Push(job))
        .map_err(|_| PushError::QueueClosed)?;
    tracing::info!(repo = %repo_id, "push queued");
    Ok(PushTicket { events, result })
}

/// Runs one queued Push and sends its outcome.
pub(crate) async fn run_job(env: &AcceptEnv, job: PushJob) {
    let result = if job.events.is_closed() {
        Ok(PushOutcome::Cancelled)
    } else {
        push(env, &job.request, &job.events).await
    };
    match &result {
        Ok(PushOutcome::Pushed(report)) => {
            tracing::info!(head = %report.head, upstream = %report.upstream, commit = %report.commit, "push finished");
        }
        Ok(PushOutcome::UpToDate(report)) => {
            tracing::info!(head = %report.head, "nothing to push");
        }
        Ok(PushOutcome::Conflict(conflict)) => {
            tracing::info!(files = conflict.files.len(), "pull before push conflicts");
        }
        Ok(PushOutcome::Cancelled) => tracing::info!("push cancelled"),
        Err(error) => tracing::warn!(%error, "push failed"),
    }
    let _ = job.result.send(result);
}

fn step(events: &mpsc::UnboundedSender<PushEvent>, step: PushStep) {
    let _ = events.send(PushEvent::Step(step));
}

fn log(events: &mpsc::UnboundedSender<PushEvent>, entry: &StepLog) {
    let _ = events.send(PushEvent::Log(entry.clone()));
}

/// One Push (see the module docs).
async fn push(
    env: &AcceptEnv,
    request: &PushRequest,
    events: &mpsc::UnboundedSender<PushEvent>,
) -> Result<PushOutcome, PushError> {
    let services = &env.services;
    let repo = &request.repo;
    let head = request.head.as_str();
    let (upstream, set_upstream) = match &request.mode {
        PushMode::Upstream => match git_push::upstream_of(repo, head).await? {
            Some(upstream) => (upstream, false),
            None => return Err(PushError::NoUpstream(head.to_owned())),
        },
        PushMode::SetUpstream { remote } => (
            Upstream {
                remote: remote.clone(),
                branch: head.to_owned(),
            },
            true,
        ),
    };
    let upstream_name = upstream.to_string();
    let mut rebased: Option<(Sha, Sha)> = None;
    let mut retried = false;

    for attempt in 0..ATTEMPTS {
        if events.is_closed() {
            return Ok(PushOutcome::Cancelled);
        }
        if attempt > 0 {
            retried = true;
            step(events, PushStep::Retrying);
        }
        let mut local = match write::branch_tip(repo, head).await {
            Ok(tip) => tip,
            Err(GitError::RefNotFound { .. }) => {
                return Err(PushError::BranchGone(head.to_owned()));
            }
            Err(error) => return Err(error.into()),
        };
        let integration = services
            .worktrees
            .integration_worktree(repo, &local)
            .await?;
        let dir = integration.path();

        // 1. Fetch the upstream branch only.
        step(events, PushStep::Fetching);
        let (fetch_log, fetched) = git_push::fetch_upstream(dir, &upstream).await?;
        log(events, &fetch_log);
        let remote_tip = match fetched {
            FetchOutcome::Fetched(tip) => Some(tip),
            FetchOutcome::Missing => None,
            FetchOutcome::Failed => return Err(PushError::step(&fetch_log)),
        };

        // 2. Pull with rebase when the remote has commits head lacks.
        if let Some(remote_tip) = remote_tip {
            let (_, behind) = git_push::ahead_behind(dir, &local, &remote_tip).await?;
            if behind > 0 {
                if !confirm_checked_out(env, request, events).await? {
                    return Ok(PushOutcome::Cancelled);
                }
                step(events, PushStep::Rebasing);
                let (outcome, rebase_log) =
                    services.worktrees.rebase(&integration, &remote_tip).await?;
                log(events, &rebase_log);
                let new = match outcome {
                    RebaseOutcome::Rebased(new) => new,
                    RebaseOutcome::Conflict { files } => {
                        services.worktrees.reset(&integration, &local).await?;
                        return Ok(PushOutcome::Conflict(PushConflict {
                            operation: PULL_BEFORE_PUSH,
                            head: head.to_owned(),
                            upstream: upstream_name,
                            head_tip: local,
                            upstream_tip: remote_tip,
                            files,
                            detail: rebase_log.output,
                        }));
                    }
                    RebaseOutcome::Failed => return Err(PushError::step(&rebase_log)),
                };
                step(events, PushStep::MovingBranch);
                match write::update_branch(repo, head, &new, &local, REFLOG_REASON).await? {
                    RefUpdate::Moved => {
                        let first = rebased.map_or(local, |(from, _)| from);
                        rebased = Some((first, new));
                        local = new;
                    }
                    RefUpdate::Stale { current: None } => {
                        return Err(PushError::BranchGone(head.to_owned()));
                    }
                    RefUpdate::Stale { current: Some(_) } => {
                        if attempt + 1 < ATTEMPTS {
                            tracing::info!(head, "head moved during the rebase; retrying");
                            continue;
                        }
                        return Err(PushError::BranchMoved {
                            head: head.to_owned(),
                        });
                    }
                }
            }
            let (ahead, behind) = git_push::ahead_behind(dir, &local, &remote_tip).await?;
            if ahead == 0 && !set_upstream {
                return Ok(PushOutcome::UpToDate(PushReport {
                    head: head.to_owned(),
                    upstream: upstream_name,
                    commit: local,
                    rebased,
                    retried,
                    set_upstream,
                    ahead,
                    behind,
                }));
            }
        }

        if events.is_closed() {
            return Ok(PushOutcome::Cancelled);
        }

        // 3. Push, never forced.
        step(events, PushStep::Pushing);
        let (push_log, pushed) = git_push::push_branch(dir, &upstream, head, set_upstream).await?;
        log(events, &push_log);
        match pushed {
            PushResult::Pushed => {
                let (ahead, behind) = match git_push::ref_tip(dir, &upstream.tracking_ref()).await?
                {
                    Some(tip) => git_push::ahead_behind(dir, &local, &tip).await?,
                    None => (git_push::commit_count(dir, &local).await?, 0),
                };
                return Ok(PushOutcome::Pushed(PushReport {
                    head: head.to_owned(),
                    upstream: upstream_name,
                    commit: local,
                    rebased,
                    retried,
                    set_upstream,
                    ahead,
                    behind,
                }));
            }
            PushResult::Rejected if attempt + 1 < ATTEMPTS => {
                tracing::info!(head, "push rejected; fetching again");
            }
            PushResult::Rejected => {
                return Err(PushError::Rejected {
                    output: push_log.output,
                });
            }
            PushResult::Failed => return Err(PushError::step(&push_log)),
        }
    }
    Err(PushError::BranchMoved {
        head: head.to_owned(),
    })
}

/// Asks before the rebase moves a head branch checked out in one of the
/// user's checkouts, unless the user already agreed for this repository.
/// `true` to go on.
async fn confirm_checked_out(
    env: &AcceptEnv,
    request: &PushRequest,
    events: &mpsc::UnboundedSender<PushEvent>,
) -> Result<bool, PushError> {
    let services = &env.services;
    if checked_out_acknowledged(services, request.repo_id).await? {
        return Ok(true);
    }
    let Some(path) = user_checkout_of(services, &request.repo, &request.head).await? else {
        return Ok(true);
    };
    let (reply, answer) = oneshot::channel();
    if events
        .send(PushEvent::CheckedOut {
            head: request.head.clone(),
            path,
            reply,
        })
        .is_err()
    {
        return Ok(false);
    }
    if answer.await != Ok(true) {
        return Ok(false);
    }
    acknowledge_checked_out(services, request.repo_id).await?;
    Ok(true)
}
