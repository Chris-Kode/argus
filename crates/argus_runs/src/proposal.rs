//! Proposals: what a finished run changed, as its own diff, and the
//! decisions on it (spec §6.5).
//!
//! [`open`] snapshots the run's worktree as a tree object
//! ([`WorktreeManager::snapshot_tree`]: `git add -A` + `git write-tree`), so
//! the proposal is an ordinary two-sided diff `start_sha → tree`, with new
//! and deleted files, rendered by the same viewer as the main diff. Files
//! other than the commented one are listed first ([`DiffSession::scoped`]).
//! [`refresh`] takes a new snapshot after a manual edit, and [`watch`]
//! reports such edits. [`reject`] deletes the worktree, records the run as
//! `Rejected` and moves the comment back to `Open`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use argus_diff::review::{Comment, CommentEvent, ReviewError, apply_event};
use argus_diff::{DiffOptions, DiffSession};
use argus_git::{GitError, Repo, Sha, Worktree, read};
use argus_store::{CommentId, CommentStatus, Run, RunId, RunStatus, StoreError};
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecursiveMode, Watcher as _};
use tokio::sync::mpsc;

use crate::run::{RunUpdate, Services};

/// How often a worktree is polled when no file watcher can be installed.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Why a proposal cannot be shown or decided on.
#[derive(Debug, thiserror::Error)]
pub enum ProposalError {
    /// The run does not exist.
    #[error("run {0} does not exist")]
    RunNotFound(RunId),
    /// The comment has no run whose change can be reviewed.
    #[error("comment {0} has no proposal to review")]
    NoProposal(CommentId),
    /// The run works on no comment, so its repository is unknown.
    #[error("run {0} has no comment")]
    NoComment(RunId),
    /// The run's comment no longer exists.
    #[error("the comment of run {0} no longer exists")]
    CommentNotFound(RunId),
    /// The comment's repository is not recorded.
    #[error("the repository of run {0} is not recorded")]
    RepoNotFound(RunId),
    /// The run is in a status without a reviewable change.
    #[error("run {run_id} is {status}; it has no proposal to review")]
    NotReviewable {
        /// The run.
        run_id: RunId,
        /// Its status.
        status: RunStatus,
    },
    /// The run's worktree was deleted.
    #[error("the worktree of run {0} no longer exists")]
    WorktreeGone(RunId),
    /// A git operation failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The comment status could not change.
    #[error(transparent)]
    Review(#[from] ReviewError),
}

/// A run's proposed change, ready to be shown.
///
/// Cheap to clone; the diff session is shared.
#[derive(Clone, Debug)]
pub struct Proposal {
    /// The run, as stored when the proposal was opened.
    pub run: Run,
    /// The comment the run worked on (without its replies).
    pub comment: Comment,
    /// The repository.
    pub repo: Repo,
    /// The run's worktree.
    pub worktree: Worktree,
    /// The head commit the run started from.
    pub start_sha: Sha,
    /// The worktree's state, written as a tree object.
    pub tree: Sha,
    /// The tip of the comment's head branch now; `None` when it cannot be
    /// resolved (e.g. the branch was deleted).
    pub head_tip: Option<Sha>,
    /// `start_sha → tree`, with files outside the comment's file first.
    pub session: DiffSession,
}

impl Proposal {
    /// Whether the head branch moved since the run started (e.g. another
    /// Accept landed); Accept then applies on top of the new tip.
    pub fn head_moved(&self) -> bool {
        self.head_tip.is_some_and(|tip| tip != self.start_sha)
    }

    /// The paths the comment covers: its file, and its old path for a
    /// rename.
    pub fn scope(&self) -> Vec<String> {
        scope_of(&self.comment)
    }

    /// Absolute path of `path` (relative to the repository root) in the
    /// worktree.
    pub fn worktree_file(&self, path: &str) -> PathBuf {
        self.worktree.path().join(path)
    }
}

fn scope_of(comment: &Comment) -> Vec<String> {
    std::iter::once(comment.anchor.file_path.clone())
        .chain(comment.anchor.old_file_path.clone())
        .collect()
}

/// Whether a run in `status` holds a change to review: a successful run, or
/// a stopped one whose partial change was kept.
fn reviewable(status: RunStatus) -> bool {
    matches!(status, RunStatus::Succeeded | RunStatus::Stopped)
}

/// The latest run of `comment_id` with a change to review.
pub(crate) async fn latest_run(
    services: &Services,
    comment_id: CommentId,
) -> Result<RunId, ProposalError> {
    services
        .store
        .runs()
        .list_for_comment(comment_id)
        .await?
        .into_iter()
        .rev()
        .find(|run| reviewable(run.status))
        .map(|run| run.id)
        .ok_or(ProposalError::NoProposal(comment_id))
}

/// The run, its comment, repository and worktree, checked for review.
async fn load_parts(
    services: &Services,
    run_id: RunId,
) -> Result<(Run, Comment, Repo, Worktree), ProposalError> {
    let store = &services.store;
    let run = store
        .runs()
        .get(run_id)
        .await?
        .ok_or(ProposalError::RunNotFound(run_id))?;
    if !reviewable(run.status) {
        return Err(ProposalError::NotReviewable {
            run_id,
            status: run.status,
        });
    }
    let comment_id = run.comment_id.ok_or(ProposalError::NoComment(run_id))?;
    let stored = store
        .comments()
        .get(comment_id)
        .await?
        .ok_or(ProposalError::CommentNotFound(run_id))?;
    let record = store
        .repos()
        .get(stored.repo_id)
        .await?
        .ok_or(ProposalError::RepoNotFound(run_id))?;
    let repo = services.git.open(record.path).await?;
    let worktree = services
        .worktrees
        .worktree_at(&repo, run.worktree_path.clone())?;
    Ok((
        run,
        Comment::from_stored(stored, Vec::new()),
        repo,
        worktree,
    ))
}

/// Opens the proposal of `run_id`: snapshots its worktree and prepares the
/// diff. Runs on the tokio runtime.
pub(crate) async fn open(services: &Services, run_id: RunId) -> Result<Proposal, ProposalError> {
    let (run, comment, repo, worktree) = load_parts(services, run_id).await?;
    if !worktree.path().is_dir() {
        return Err(ProposalError::WorktreeGone(run_id));
    }
    let start_sha = Sha::parse(&run.start_sha).ok_or_else(|| GitError::Parse {
        command: "runs.start_sha".to_owned(),
        message: format!("not an object id: `{}`", run.start_sha),
    })?;
    let tree = services.worktrees.snapshot_tree(&worktree).await?;
    let head_tip = head_tip(&repo, &comment.head_ref).await;
    let session = DiffSession::scoped(
        &repo,
        start_sha,
        tree,
        DiffOptions::default(),
        scope_of(&comment),
    );
    tracing::debug!(run = %run_id, %start_sha, %tree, "opened proposal");
    Ok(Proposal {
        run,
        comment,
        repo,
        worktree,
        start_sha,
        tree,
        head_tip,
        session,
    })
}

/// Takes a new snapshot of the proposal's worktree. `None` when neither the
/// worktree nor the head branch changed, so the shown diff stays.
pub(crate) async fn refresh(
    services: &Services,
    proposal: &Proposal,
) -> Result<Option<Proposal>, ProposalError> {
    if !proposal.worktree.path().is_dir() {
        return Err(ProposalError::WorktreeGone(proposal.run.id));
    }
    let tree = services.worktrees.snapshot_tree(&proposal.worktree).await?;
    let head_tip = head_tip(&proposal.repo, &proposal.comment.head_ref).await;
    if tree == proposal.tree && head_tip == proposal.head_tip {
        return Ok(None);
    }
    let session = if tree == proposal.tree {
        proposal.session.clone()
    } else {
        tracing::debug!(run = %proposal.run.id, %tree, "proposal changed on disk");
        DiffSession::scoped(
            &proposal.repo,
            proposal.start_sha,
            tree,
            DiffOptions::default(),
            proposal.scope(),
        )
    };
    Ok(Some(Proposal {
        tree,
        head_tip,
        session,
        ..proposal.clone()
    }))
}

async fn head_tip(repo: &Repo, head_ref: &str) -> Option<Sha> {
    match read::resolve(repo, head_ref).await {
        Ok(tip) => Some(tip),
        Err(error) => {
            tracing::debug!(head_ref, %error, "cannot resolve the head branch");
            None
        }
    }
}

/// Rejects the proposal of `run_id`: `git worktree remove --force` + `git
/// worktree prune`, the run becomes `Rejected` (it stays in history), and a
/// `Proposed` comment goes back to `Open`.
pub(crate) async fn reject(services: &Services, run_id: RunId) -> Result<(), ProposalError> {
    if services.is_active(run_id) {
        return Err(ProposalError::NotReviewable {
            run_id,
            status: RunStatus::Running,
        });
    }
    let (run, comment, _, worktree) = load_parts(services, run_id).await?;
    // The worktree goes first: a failed removal leaves the proposal intact.
    services.worktrees.remove(&worktree).await?;
    let runs = services.store.runs();
    if !runs
        .set_status_if(run_id, run.status, RunStatus::Rejected)
        .await?
    {
        let status = runs.get(run_id).await?.map_or(run.status, |run| run.status);
        return Err(ProposalError::NotReviewable { run_id, status });
    }
    let comments = services.store.comments();
    if comment.status == CommentStatus::Proposed {
        apply_event(&comments, comment.id, CommentEvent::Rejected).await?;
    }
    tracing::info!(run = %run_id, comment = %comment.id, "rejected proposal");
    services.publish(RunUpdate::Rejected {
        run_id,
        comment_id: Some(comment.id),
    });
    Ok(())
}

/// Reports changes to the files of a worktree, for a proposal that refreshes
/// after a manual edit. Dropping it stops watching.
#[derive(Debug)]
pub struct WorktreeWatch {
    changes: mpsc::Receiver<()>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl WorktreeWatch {
    /// Resolves at the next change (several changes in a row may resolve it
    /// once). `None` when the watch ended.
    pub async fn changed(&mut self) -> Option<()> {
        self.changes.recv().await
    }

    /// Drops the changes reported so far.
    pub fn drain(&mut self) {
        while self.changes.try_recv().is_ok() {}
    }
}

/// Watches the files of `path` recursively. Without a working file watcher
/// (e.g. the inotify limit is reached), it reports a possible change every
/// [`POLL_INTERVAL`] instead; [`refresh`] then finds out whether anything
/// moved. Must be called on the tokio runtime.
pub(crate) fn watch(path: &Path) -> WorktreeWatch {
    // One pending notification is enough: the reader takes a fresh snapshot.
    let (tx, changes) = mpsc::channel(1);
    let notify_tx = tx.clone();
    let watcher =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(event) if is_change(&event.kind) => {
                let _ = notify_tx.try_send(());
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(%error, "worktree watch error"),
        })
        .and_then(|mut watcher| {
            watcher.watch(path, RecursiveMode::Recursive)?;
            Ok(watcher)
        });
    match watcher {
        Ok(watcher) => WorktreeWatch {
            changes,
            _watcher: Some(watcher),
        },
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot watch the worktree; polling it");
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(POLL_INTERVAL);
                while !tx.is_closed() {
                    interval.tick().await;
                    let _ = tx.try_send(());
                }
            });
            WorktreeWatch {
                changes,
                _watcher: None,
            }
        }
    }
}

/// Whether a watcher event may change the worktree's content. Reads (which
/// `git add -A` itself causes) are not changes.
fn is_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}
