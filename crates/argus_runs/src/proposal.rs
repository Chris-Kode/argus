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
//!
//! A conflict resolution ([`crate::resolve`]) is a proposal too. One for an
//! Accept conflict belongs to the comment; rejecting it leaves the comment
//! `Proposed` with its original proposal. One for a pull-before-push rebase
//! has no comment: its repository and head branch come from the
//! resolution in progress, and rejecting it cancels that resolution (the
//! Push is abandoned). Either carries the conflict markers still in its
//! worktree ([`Proposal::markers`]), which block its Accept.

use std::path::{Path, PathBuf};
use std::time::Duration;

use argus_diff::review::{Comment, CommentEvent, ReviewError, apply_event};
use argus_diff::{DiffOptions, DiffSession};
use argus_git::{ConflictMarker, GitError, Repo, Sha, Worktree, read};
use argus_store::{CommentId, CommentStatus, Run, RunId, RunStatus, StoreError};
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecursiveMode, Watcher as _};
use tokio::sync::mpsc;

use crate::resolve::Resolution;
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
    /// The pull-before-push resolution of a comment-less run is no longer
    /// in progress (cancelled, failed, or Argus restarted).
    #[error(
        "the pull before push this resolution belongs to is no longer in progress; Push again \
         to start over"
    )]
    ResolveGone(RunId),
    /// Cancelling a pull-before-push resolution failed.
    #[error("{0}")]
    Resolve(String),
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
    /// The comment the run worked on (without its replies); `None` for the
    /// resolution of a pull-before-push rebase.
    pub comment: Option<Comment>,
    /// The head branch the change lands on.
    pub head: String,
    /// What the proposal resolves, when it is a conflict resolution.
    pub resolution: Option<Resolution>,
    /// The conflict markers left in the worktree (resolutions only); while
    /// there are any, Accept is refused.
    pub markers: Vec<ConflictMarker>,
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

/// The run, its comment (if any), repository, worktree, head branch and
/// resolution, checked for review.
struct Parts {
    run: Run,
    comment: Option<Comment>,
    repo: Repo,
    worktree: Worktree,
    head: String,
    resolution: Option<Resolution>,
}

impl Proposal {
    /// Whether the head branch moved since the run started (e.g. another
    /// Accept landed); Accept then applies on top of the new tip.
    pub fn head_moved(&self) -> bool {
        self.head_tip.is_some_and(|tip| tip != self.start_sha)
    }

    /// The paths the comment covers: its file, and its old path for a
    /// rename. Empty without a comment.
    pub fn scope(&self) -> Vec<String> {
        self.comment.as_ref().map(scope_of).unwrap_or_default()
    }

    /// Whether this resolution resolves a pull-before-push rebase: its
    /// Accept continues the rebase and the Push.
    pub fn is_rebase_resolution(&self) -> bool {
        matches!(self.resolution, Some(Resolution::Rebase(_)))
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
pub(crate) async fn load_parts(
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

/// [`load_parts`] for any reviewable run, with or without a comment.
async fn load_any(services: &Services, run_id: RunId) -> Result<Parts, ProposalError> {
    let run = services
        .store
        .runs()
        .get(run_id)
        .await?
        .ok_or(ProposalError::RunNotFound(run_id))?;
    if run.comment_id.is_some() {
        let (run, comment, repo, worktree) = load_parts(services, run_id).await?;
        let resolution = crate::resolve::resolution_of(services, &run).await?;
        return Ok(Parts {
            head: comment.head_ref.clone(),
            run,
            comment: Some(comment),
            repo,
            worktree,
            resolution,
        });
    }
    if !reviewable(run.status) {
        return Err(ProposalError::NotReviewable {
            run_id,
            status: run.status,
        });
    }
    let (repo, head) = crate::resolve::rebase_target(services, &run.worktree_path)
        .ok_or(ProposalError::ResolveGone(run_id))?;
    let worktree = services
        .worktrees
        .worktree_at(&repo, run.worktree_path.clone())?;
    let resolution = crate::resolve::resolution_of(services, &run).await?;
    Ok(Parts {
        run,
        comment: None,
        repo,
        worktree,
        head,
        resolution,
    })
}

/// The conflict markers left in a resolution's worktree; none for other
/// proposals.
async fn markers_of(
    services: &Services,
    worktree: &Worktree,
    start_sha: &Sha,
    resolution: Option<&Resolution>,
) -> Result<Vec<ConflictMarker>, GitError> {
    let scan = match resolution {
        None => return Ok(Vec::new()),
        Some(Resolution::Accept) => Vec::new(),
        Some(Resolution::Rebase(rebase)) => rebase.files.clone(),
    };
    services
        .worktrees
        .conflict_markers(worktree, start_sha, &scan)
        .await
}

fn diff_session(repo: &Repo, start_sha: Sha, tree: Sha, comment: Option<&Comment>) -> DiffSession {
    match comment {
        Some(comment) => DiffSession::scoped(
            repo,
            start_sha,
            tree,
            DiffOptions::default(),
            scope_of(comment),
        ),
        None => DiffSession::between(repo, start_sha, tree, DiffOptions::default()),
    }
}

/// Opens the proposal of `run_id`: snapshots its worktree and prepares the
/// diff. Runs on the tokio runtime.
pub(crate) async fn open(services: &Services, run_id: RunId) -> Result<Proposal, ProposalError> {
    let Parts {
        run,
        comment,
        repo,
        worktree,
        head,
        resolution,
    } = load_any(services, run_id).await?;
    if !worktree.path().is_dir() {
        return Err(ProposalError::WorktreeGone(run_id));
    }
    let start_sha = Sha::parse(&run.start_sha).ok_or_else(|| GitError::Parse {
        command: "runs.start_sha".to_owned(),
        message: format!("not an object id: `{}`", run.start_sha),
    })?;
    let tree = services.worktrees.snapshot_tree(&worktree).await?;
    let markers = markers_of(services, &worktree, &start_sha, resolution.as_ref()).await?;
    let head_tip = head_tip(&repo, &head).await;
    let session = diff_session(&repo, start_sha, tree, comment.as_ref());
    tracing::debug!(run = %run_id, %start_sha, %tree, "opened proposal");
    Ok(Proposal {
        run,
        comment,
        head,
        resolution,
        markers,
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
    let head_tip = head_tip(&proposal.repo, &proposal.head).await;
    if tree == proposal.tree && head_tip == proposal.head_tip {
        return Ok(None);
    }
    let (session, markers) = if tree == proposal.tree {
        (proposal.session.clone(), proposal.markers.clone())
    } else {
        tracing::debug!(run = %proposal.run.id, %tree, "proposal changed on disk");
        let markers = markers_of(
            services,
            &proposal.worktree,
            &proposal.start_sha,
            proposal.resolution.as_ref(),
        )
        .await?;
        let session = diff_session(
            &proposal.repo,
            proposal.start_sha,
            tree,
            proposal.comment.as_ref(),
        );
        (session, markers)
    };
    Ok(Some(Proposal {
        tree,
        head_tip,
        session,
        markers,
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
/// worktree prune`, the run becomes `Rejected` (it stays in history), as do
/// the earlier runs of the thread that shared the worktree, and a `Proposed`
/// comment goes back to `Open`. Refused while a follow-up is running.
pub(crate) async fn reject(services: &Services, run_id: RunId) -> Result<(), ProposalError> {
    if services.is_active(run_id) {
        return Err(ProposalError::NotReviewable {
            run_id,
            status: RunStatus::Running,
        });
    }
    let stored = services
        .store
        .runs()
        .get(run_id)
        .await?
        .ok_or(ProposalError::RunNotFound(run_id))?;
    if stored.comment_id.is_none() {
        // A pull-before-push resolution: rejecting it cancels the
        // resolution and abandons the Push.
        let (id, _) = services
            .resolves
            .find_by_worktree(&stored.worktree_path)
            .ok_or(ProposalError::ResolveGone(run_id))?;
        return crate::resolve::cancel_rebase(services, id)
            .await
            .map_err(|error| ProposalError::Resolve(error.to_string()));
    }
    let (run, comment, _, worktree) = load_parts(services, run_id).await?;
    let resolution = crate::resolve::resolution_of(services, &run).await?;
    // A follow-up may be editing the same worktree.
    if comment.status == CommentStatus::Running {
        return Err(ProposalError::NotReviewable {
            run_id,
            status: RunStatus::Running,
        });
    }
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
    // Earlier runs of the thread (follow-ups) share the deleted worktree:
    // none of them has a proposal any more.
    for other in runs.find_by_worktree(&run.worktree_path).await? {
        if other.id != run_id && reviewable(other.status) {
            runs.set_status_if(other.id, other.status, RunStatus::Rejected)
                .await?;
        }
    }
    let comments = services.store.comments();
    // A rejected resolution leaves the comment's original proposal (and the
    // comment `Proposed`).
    if comment.status == CommentStatus::Proposed && resolution.is_none() {
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
