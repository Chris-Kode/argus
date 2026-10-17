//! Resolve runs: Claude resolves the conflict of an Accept or of a pull
//! before push, and the result goes through the review loop (spec §6.6,
//! §11).
//!
//! A conflict never leaves half-applied state behind: Accept and Push reset
//! the integration worktree as soon as they conflict, and open the conflict
//! dialog. **Resolve with Claude** rebuilds the conflicted state in a new
//! run worktree under `~/.argus/worktrees/` and starts a run there (`kind =
//! resolve`, the same allowed tools):
//!
//! - **Accept conflict** ([`RunManager::resolve_accept`](crate::RunManager::resolve_accept)):
//!   the worktree is detached at the current head tip and the proposal's
//!   patch applied with `git apply --3way`, so the markers are in the files.
//!   The run is linked to the comment, which goes `Proposed → Running →
//!   Proposed`. Its proposal (`head tip → worktree`) is accepted like any
//!   other ([`crate::accept`]), with the comment's commit subject; the
//!   comment's earlier worktrees are deleted then. Rejecting it deletes the
//!   resolve worktree only: the comment's original proposal stays.
//! - **Rebase conflict** ([`RunManager::resolve_push`](crate::RunManager::resolve_push)):
//!   the worktree is detached at the local head and `git rebase <upstream>`
//!   runs there until it stops at the first conflicting commit. The run has
//!   no comment; this pull-before-push resolution lives in memory
//!   ([`ResolveSessions`]) with the Push it belongs to. Accepting its
//!   proposal ([`RunManager::continue_push`](crate::RunManager::continue_push))
//!   runs `git add -A && git rebase --continue` there: the next conflicting
//!   commit opens the dialog again (one resolve run per stopped commit, in
//!   the same worktree); a finished rebase moves the head branch with a
//!   compare-and-swap and the Push goes on. Cancel or Reject deletes the
//!   worktree and abandons the Push.
//!
//! Before any Accept of a resolution, the worktree must hold no conflict
//! marker (`git diff --check` + a scan of the conflicted files); otherwise
//! the Accept is refused with the markers' locations.
//!
//! Nothing is rebased onto the head branch or committed until the user
//! accepts; Cancel at any point leaves the head branch, the remote and the
//! user's checkout unchanged.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use argus_diff::review::CommentEvent;
use argus_git::conflicts::is_relative_inside;
use argus_git::write;
use argus_git::{ConflictedApply, GitError, RebaseStep, RebaseStop, Repo, Sha, Worktree};
use argus_store::{CommentStatus, RunId, RunKind, RunStatus};
use uuid::Uuid;

use crate::accept::AcceptConflict;
use crate::claude::{ClaudeCapabilities, SessionMode};
use crate::prompt::{
    ConflictOperation, ConflictedFile, RepoContext, ResolveContext, resolve_prompt,
};
use crate::proposal::{ProposalError, load_parts};
use crate::push::{PushConflict, PushRequest};
use crate::run::{
    PendingRun, RunError, RunUpdate, Services, blocking, claim_comment, comment_context,
    load_comment_target, record_run, repo_context, spawn,
};

/// Largest part of a conflicted file put in the resolve prompt, in bytes;
/// Claude reads the rest from the file.
const FILE_PROMPT_MAX_BYTES: usize = 64 * 1024;

/// Largest total of conflicted file contents in the resolve prompt, in
/// bytes.
const PROMPT_FILES_MAX_BYTES: usize = 256 * 1024;

/// Identifies a pull-before-push resolution in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResolveId(Uuid);

impl ResolveId {
    fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl fmt::Display for ResolveId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// What a proposal resolves, when it is a conflict resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The conflict of the Accept of the comment's proposal: Accept commits
    /// it like a normal Accept, with the comment's subject.
    Accept,
    /// One stopped commit of a pull-before-push rebase: Accept continues the
    /// rebase, then the Push.
    Rebase(RebaseResolution),
}

/// Where a pull-before-push resolution is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseResolution {
    /// The resolution in progress.
    pub id: ResolveId,
    /// The head branch.
    pub head: String,
    /// The upstream, `origin/feature`.
    pub upstream: String,
    /// The local commit the rebase stopped at.
    pub commit: Sha,
    /// Its subject line.
    pub subject: String,
    /// The conflicted files of that commit.
    pub files: Vec<PathBuf>,
    /// How many commits were resolved before this one.
    pub resolved: u32,
}

/// A pull-before-push rebase stopped in its resolve worktree.
#[derive(Clone, Debug)]
pub(crate) struct RebaseSession {
    /// The Push it belongs to; it goes on once the rebase finishes.
    pub request: PushRequest,
    /// The upstream, `origin/feature`.
    pub upstream: String,
    /// The remote tip the rebase targets.
    pub upstream_tip: Sha,
    /// The local head tip the rebase started from: the expected old value
    /// of the compare-and-swap.
    pub expected_head: Sha,
    /// The resolve worktree, shared by every resolve run of the rebase.
    pub worktree: Worktree,
    /// Where the rebase is stopped.
    pub stop: RebaseStop,
    /// Commits resolved so far.
    pub resolved: u32,
}

impl RebaseSession {
    pub fn resolution(&self, id: ResolveId) -> RebaseResolution {
        RebaseResolution {
            id,
            head: self.request.head.clone(),
            upstream: self.upstream.clone(),
            commit: self.stop.commit,
            subject: self.stop.subject.clone(),
            files: self.stop.files.clone(),
            resolved: self.resolved,
        }
    }
}

/// The pull-before-push resolutions in progress. They live in memory: after
/// a restart their worktrees are orphans, removed by the startup
/// reconciliation.
#[derive(Debug, Default)]
pub(crate) struct ResolveSessions {
    rebases: Mutex<HashMap<ResolveId, RebaseSession>>,
}

impl ResolveSessions {
    fn lock(&self) -> MutexGuard<'_, HashMap<ResolveId, RebaseSession>> {
        self.rebases.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get(&self, id: ResolveId) -> Option<RebaseSession> {
        self.lock().get(&id).cloned()
    }

    /// The resolution whose worktree is `path`.
    pub fn find_by_worktree(&self, path: &Path) -> Option<(ResolveId, RebaseSession)> {
        self.lock()
            .iter()
            .find(|(_, session)| same_path(session.worktree.path(), path))
            .map(|(id, session)| (*id, session.clone()))
    }

    fn insert(&self, id: ResolveId, session: RebaseSession) {
        self.lock().insert(id, session);
    }

    /// Records the next stopped commit.
    pub fn advance(&self, id: ResolveId, stop: RebaseStop) {
        if let Some(session) = self.lock().get_mut(&id) {
            session.stop = stop;
            session.resolved += 1;
        }
    }

    pub fn remove(&self, id: ResolveId) -> Option<RebaseSession> {
        self.lock().remove(&id)
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .zip(std::fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

/// The model, effort and CLI of a resolve run.
pub(crate) struct ResolveRequest {
    pub model: String,
    pub effort: String,
    pub capabilities: ClaudeCapabilities,
    pub rules_path: PathBuf,
}

/// Starts the resolve run of an Accept conflict and returns once its
/// process runs (see the module docs).
pub(crate) async fn resolve_accept(
    services: Arc<Services>,
    conflict: AcceptConflict,
    request: ResolveRequest,
) -> Result<RunId, RunError> {
    let original = conflict.run_id;
    if services.is_active(original) {
        return Err(RunError::Busy(conflict.comment_id));
    }
    let (run, comment, repo, original_worktree) = load_parts(&services, original).await?;
    if !original_worktree.path().is_dir() {
        return Err(ProposalError::WorktreeGone(original).into());
    }
    match comment.status {
        CommentStatus::Proposed | CommentStatus::Open => {}
        CommentStatus::Running => return Err(RunError::Busy(comment.id)),
        status => {
            return Err(RunError::ConflictGone(format!(
                "the comment is {status}; there is nothing to resolve"
            )));
        }
    }
    let start_sha = parse_sha(&run.start_sha)?;
    let patch = services
        .worktrees
        .diff_against_start(&original_worktree, &start_sha)
        .await?;
    if patch.is_empty() {
        return Err(RunError::ConflictGone(
            "the proposal is empty; there is nothing to resolve".to_owned(),
        ));
    }
    let head = comment.head_ref.clone();
    let tip = match write::branch_tip(&repo, &head).await {
        Ok(tip) => tip,
        Err(GitError::RefNotFound { .. }) => {
            return Err(RunError::ConflictGone(format!(
                "the head branch `{head}` no longer exists"
            )));
        }
        Err(error) => return Err(error.into()),
    };
    let target = load_comment_target(&services, comment.id, true).await?;
    let (_, build_context) = comment_context(&target).await?;

    // The worktree comes first, so a patch that applies cleanly by now
    // leaves no run behind.
    let run_id = RunId::new();
    let worktree = services
        .worktrees
        .create_run_worktree(&repo, &tip, run_id)
        .await?;
    let files = match services
        .worktrees
        .apply_keeping_conflicts(&worktree, &patch.bytes)
        .await
    {
        Ok(ConflictedApply::Conflicted { files }) => files,
        Ok(ConflictedApply::Clean) => {
            remove_quietly(&services, &worktree).await;
            return Err(RunError::ConflictGone(format!(
                "the proposal now applies cleanly on `{head}`; accept it again"
            )));
        }
        Err(error) => {
            remove_quietly(&services, &worktree).await;
            return Err(error.into());
        }
    };
    let conflicted_files = read_conflicted(&worktree, &files).await;
    let repo_context = repo_context(&target, &tip.to_hex());
    let comment_label = first_line(&comment.body_md);
    let prompt = blocking(move || {
        resolve_prompt(
            &repo_context,
            &ResolveContext {
                operation: ConflictOperation::Accept { comment_label },
                conflicted_files,
                original_comment: Some(build_context()),
            },
        )
    })
    .await;
    let prompt = match prompt {
        Ok(prompt) => prompt,
        Err(error) => {
            remove_quietly(&services, &worktree).await;
            return Err(error);
        }
    };

    let pending = PendingRun {
        run_id,
        comment_id: Some(comment.id),
        kind: RunKind::Resolve,
        start_sha: tip,
        model: request.model,
        effort: request.effort,
        session: SessionMode::New(Uuid::now_v7()),
        capabilities: request.capabilities,
        rules_path: request.rules_path,
    };
    if let Err(error) = record_run(&services, &pending, worktree.path().to_owned(), &prompt).await {
        remove_quietly(&services, &worktree).await;
        return Err(error);
    }
    // `Proposed → Running` (back to `Proposed` when the run ends). The
    // partial change of a stopped run left the comment `Open`; it is moved
    // directly, since `Open → Running` is otherwise a new run.
    let claimed = match comment.status {
        CommentStatus::Proposed => claim_comment(&services, &pending, CommentEvent::FollowUp).await,
        _ => claim_open(&services, &pending, comment.id).await,
    };
    if let Err(error) = claimed {
        remove_quietly(&services, &worktree).await;
        return Err(error);
    }
    tracing::info!(run = %run_id, original = %original, files = files.len(), "starting an accept resolve run");
    let kept = worktree.clone();
    let started = spawn(services.clone(), pending, worktree, prompt.text).await;
    if started.is_err() {
        remove_quietly(&services, &kept).await;
    }
    started
}

/// `Open → Running` for a resolve run on the partial change of a stopped
/// run; deletes the unstarted run row when the comment moved meanwhile.
async fn claim_open(
    services: &Services,
    run: &PendingRun,
    comment_id: argus_store::CommentId,
) -> Result<(), RunError> {
    let comments = services.store.comments();
    if comments
        .set_status_if(comment_id, CommentStatus::Open, CommentStatus::Running)
        .await?
    {
        return Ok(());
    }
    if let Err(error) = services.store.runs().delete(run.run_id).await {
        tracing::warn!(run = %run.run_id, %error, "cannot delete unstarted run");
    }
    Err(RunError::Busy(comment_id))
}

/// Starts the resolve run of a pull-before-push rebase conflict: a new
/// resolution for a first conflict, or the next stopped commit of one in
/// progress ([`PushConflict::resolve`]).
pub(crate) async fn resolve_rebase(
    services: Arc<Services>,
    push: PushRequest,
    conflict: PushConflict,
    request: ResolveRequest,
) -> Result<RunId, RunError> {
    let run_id = RunId::new();
    let (id, session, new) = match &conflict.resolve {
        Some(progress) => {
            let session = services
                .resolves
                .get(progress.id)
                .ok_or(RunError::ResolveGone)?;
            if busy_worktree(&services, session.worktree.path()).await? {
                return Err(RunError::RunBusy);
            }
            (progress.id, session, false)
        }
        None => {
            let (id, session) = start_rebase(&services, &push, &conflict, run_id).await?;
            (id, session, true)
        }
    };
    // A continued resolution keeps the Push it started with.
    let push = if new { push } else { session.request.clone() };
    let worktree = session.worktree.clone();
    let abandon = |services: Arc<Services>, worktree: Worktree| async move {
        if new {
            services.resolves.remove(id);
            remove_quietly(&services, &worktree).await;
        }
    };

    let record = services.store.repos().get(push.repo_id).await?;
    let (repo_name, base) = match record {
        Some(record) => (record.name, record.last_base.unwrap_or_default()),
        None => (push.repo.workdir().display().to_string(), String::new()),
    };
    let stop = session.stop.clone();
    let conflicted_files = read_conflicted(&worktree, &stop.files).await;
    let repo_context = RepoContext {
        repo_name,
        base_branch: base,
        head_branch: push.head.clone(),
        start_sha: stop.head.to_hex(),
    };
    let context = ResolveContext {
        operation: ConflictOperation::Rebase {
            upstream: session.upstream.clone(),
            stopped_at: Some(format!("{} {}", stop.commit.short(8), stop.subject)),
        },
        conflicted_files,
        original_comment: None,
    };
    let prompt = match blocking(move || resolve_prompt(&repo_context, &context)).await {
        Ok(prompt) => prompt,
        Err(error) => {
            abandon(services.clone(), worktree).await;
            return Err(error);
        }
    };
    let pending = PendingRun {
        run_id,
        comment_id: None,
        kind: RunKind::Resolve,
        start_sha: stop.head,
        model: request.model,
        effort: request.effort,
        session: SessionMode::New(Uuid::now_v7()),
        capabilities: request.capabilities,
        rules_path: request.rules_path,
    };
    if let Err(error) = record_run(&services, &pending, worktree.path().to_owned(), &prompt).await {
        abandon(services.clone(), worktree).await;
        return Err(error);
    }
    tracing::info!(
        run = %run_id,
        resolve = %id,
        commit = %stop.commit,
        files = stop.files.len(),
        "starting a pull-before-push resolve run"
    );
    let started = spawn(services.clone(), pending, worktree.clone(), prompt.text).await;
    if started.is_err() {
        abandon(services, worktree).await;
    }
    started
}

/// Rebuilds the conflicted rebase in a new worktree named after the first
/// resolve run, and registers the resolution.
async fn start_rebase(
    services: &Services,
    push: &PushRequest,
    conflict: &PushConflict,
    run_id: RunId,
) -> Result<(ResolveId, RebaseSession), RunError> {
    let repo = &push.repo;
    let head = push.head.as_str();
    let local = match write::branch_tip(repo, head).await {
        Ok(tip) => tip,
        Err(GitError::RefNotFound { .. }) => {
            return Err(RunError::ConflictGone(format!(
                "the head branch `{head}` no longer exists"
            )));
        }
        Err(error) => return Err(error.into()),
    };
    let worktree = services
        .worktrees
        .create_run_worktree(repo, &local, run_id)
        .await?;
    let outcome = services
        .worktrees
        .rebase_until_conflict(&worktree, &conflict.upstream_tip)
        .await;
    let stop = match outcome {
        Ok((RebaseStep::Stopped(stop), _)) => stop,
        Ok((RebaseStep::Finished(_), _)) => {
            remove_quietly(services, &worktree).await;
            return Err(RunError::ConflictGone(format!(
                "`{head}` now rebases cleanly on `{}`; Push again",
                conflict.upstream
            )));
        }
        Ok((RebaseStep::Failed, log)) => {
            remove_quietly(services, &worktree).await;
            return Err(GitError::Command {
                command: log.command,
                code: None,
                stderr: log.output,
            }
            .into());
        }
        Err(error) => {
            remove_quietly(services, &worktree).await;
            return Err(error.into());
        }
    };
    let id = ResolveId::new();
    let session = RebaseSession {
        request: push.clone(),
        upstream: conflict.upstream.clone(),
        upstream_tip: conflict.upstream_tip,
        expected_head: local,
        worktree,
        stop,
        resolved: 0,
    };
    services.resolves.insert(id, session.clone());
    tracing::info!(resolve = %id, head, upstream = %conflict.upstream, "started a pull-before-push resolution");
    Ok((id, session))
}

/// Cancels a pull-before-push resolution: its worktree (and the rebase
/// stopped in it) is deleted, its proposals are rejected, and the Push is
/// abandoned. Refused while a resolve run works in it.
pub(crate) async fn cancel_rebase(services: &Services, id: ResolveId) -> Result<(), RunError> {
    let session = services.resolves.get(id).ok_or(RunError::ResolveGone)?;
    if busy_worktree(services, session.worktree.path()).await? {
        return Err(RunError::RunBusy);
    }
    services.worktrees.remove(&session.worktree).await?;
    services.resolves.remove(id);
    let runs = services.store.runs();
    for run in runs.find_by_worktree(session.worktree.path()).await? {
        if matches!(run.status, RunStatus::Succeeded | RunStatus::Stopped)
            && runs
                .set_status_if(run.id, run.status, RunStatus::Rejected)
                .await?
        {
            services.publish(RunUpdate::Rejected {
                run_id: run.id,
                comment_id: None,
            });
        }
    }
    tracing::info!(resolve = %id, "cancelled a pull-before-push resolution");
    Ok(())
}

/// After a resolve run ended without anything to review: deletes its
/// worktree, and abandons the pull-before-push resolution it belonged to.
pub(crate) async fn discard_failed(services: &Services, worktree: &Worktree) {
    if let Some((id, _)) = services.resolves.find_by_worktree(worktree.path()) {
        services.resolves.remove(id);
        tracing::info!(resolve = %id, "the resolve run failed; the pull before push is abandoned");
    }
    remove_quietly(services, worktree).await;
}

/// Whether a run of this session is working in the worktree at `path`.
pub(crate) async fn busy_worktree(services: &Services, path: &Path) -> Result<bool, RunError> {
    Ok(services
        .store
        .runs()
        .find_by_worktree(path)
        .await?
        .iter()
        .any(|run| run.status.is_active() && services.is_active(run.id)))
}

/// Whether the run's worktree holds a conflict resolution, and which.
pub(crate) async fn resolution_of(
    services: &Services,
    run: &argus_store::Run,
) -> Result<Option<Resolution>, argus_store::StoreError> {
    if run.comment_id.is_none() {
        return Ok(services
            .resolves
            .find_by_worktree(&run.worktree_path)
            .map(|(id, session)| Resolution::Rebase(session.resolution(id))));
    }
    let resolve = run.kind == RunKind::Resolve
        || services
            .store
            .runs()
            .find_by_worktree(&run.worktree_path)
            .await?
            .iter()
            .any(|other| other.kind == RunKind::Resolve);
    Ok(resolve.then_some(Resolution::Accept))
}

/// The repository and head branch of a pull-before-push resolution's run.
pub(crate) fn rebase_target(services: &Services, path: &Path) -> Option<(Repo, String)> {
    services
        .resolves
        .find_by_worktree(path)
        .map(|(_, session)| (session.request.repo.clone(), session.request.head.clone()))
}

/// The conflicted files with their markers, for the prompt; large files
/// are cut (Claude reads them whole from the worktree).
async fn read_conflicted(worktree: &Worktree, files: &[PathBuf]) -> Vec<ConflictedFile> {
    let mut budget = PROMPT_FILES_MAX_BYTES;
    let mut out = Vec::with_capacity(files.len());
    for path in files {
        let display = path.to_string_lossy().into_owned();
        let content = if !is_relative_inside(path) {
            "[not shown: unexpected path]".to_owned()
        } else {
            match tokio::fs::read(worktree.path().join(path)).await {
                Ok(bytes) if bytes.contains(&0) => "[binary file: not shown]".to_owned(),
                Ok(bytes) => {
                    let limit = FILE_PROMPT_MAX_BYTES.min(budget);
                    let text = String::from_utf8_lossy(&bytes);
                    if text.len() > limit {
                        let mut end = limit;
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        budget -= end;
                        format!(
                            "{}\n[truncated after {end} bytes of {}: read the file for the rest]",
                            &text[..end],
                            text.len()
                        )
                    } else {
                        budget -= text.len();
                        text.into_owned()
                    }
                }
                // Deleted on one side: git leaves no file with markers.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    "[the file is deleted in the working directory: one side deleted it, the \
                     other changed it]"
                        .to_owned()
                }
                Err(error) => format!("[cannot read the file: {error}]"),
            }
        };
        out.push(ConflictedFile {
            path: display,
            content,
        });
    }
    out
}

async fn remove_quietly(services: &Services, worktree: &Worktree) {
    if let Err(error) = services.worktrees.remove(worktree).await {
        tracing::warn!(path = %worktree.path().display(), %error, "cannot remove a resolve worktree");
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("review comment")
        .to_owned()
}

fn parse_sha(hex: &str) -> Result<Sha, GitError> {
    Sha::parse(hex).ok_or_else(|| GitError::Parse {
        command: "runs.start_sha".to_owned(),
        message: format!("not an object id: `{hex}`"),
    })
}

/// Where conflict previews write their scratch files: `<argus_home>/tmp`.
pub(crate) fn scratch_dir(argus_home: &Path) -> PathBuf {
    argus_home.join("tmp")
}

/// The conflicted content of `path` for the dialog of an Accept conflict:
/// a three-way merge of the run's start (base), the head tip (ours) and the
/// proposal (theirs), as `git apply --3way` would leave it.
pub(crate) async fn preview_accept(
    services: &Services,
    scratch: &Path,
    conflict: &AcceptConflict,
    path: &Path,
) -> Result<String, RunError> {
    let (run, comment, _, worktree) = load_parts(services, conflict.run_id).await?;
    if !is_relative_inside(path) {
        return Err(RunError::ConflictGone(format!(
            "`{}` is not a path of the repository",
            path.display()
        )));
    }
    let start = parse_sha(&run.start_sha)?;
    let dir = worktree.path();
    let base = argus_git::conflicts::blob_at(dir, &start, path).await?;
    let ours = argus_git::conflicts::blob_at(dir, &conflict.head_tip, path).await?;
    let theirs = read_regular(&worktree.path().join(path)).await;
    Ok(argus_git::conflicts::three_way_preview(
        scratch,
        &side(&comment.head_ref, ours),
        &side("base", base),
        &side("proposal", theirs),
    )
    .await?)
}

/// The conflicted content of `path` for the dialog of a pull-before-push
/// conflict: the file in the resolve worktree when the rebase waits there,
/// otherwise a three-way merge of the merge-base (base), the upstream
/// (ours) and the local head (theirs).
pub(crate) async fn preview_push(
    services: &Services,
    scratch: &Path,
    push: &PushRequest,
    conflict: &PushConflict,
    path: &Path,
) -> Result<String, RunError> {
    if !is_relative_inside(path) {
        return Err(RunError::ConflictGone(format!(
            "`{}` is not a path of the repository",
            path.display()
        )));
    }
    if let Some(progress) = &conflict.resolve {
        let session = services
            .resolves
            .get(progress.id)
            .ok_or(RunError::ResolveGone)?;
        let file = session.worktree.path().join(path);
        return Ok(match read_regular(&file).await {
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            None => "[the file is deleted in the working directory: one side deleted it, the \
                     other changed it]"
                .to_owned(),
        });
    }
    let dir = push.repo.workdir();
    let base =
        match argus_git::conflicts::merge_base(dir, &conflict.head_tip, &conflict.upstream_tip)
            .await?
        {
            Some(base) => argus_git::conflicts::blob_at(dir, &base, path).await?,
            None => None,
        };
    let ours = argus_git::conflicts::blob_at(dir, &conflict.upstream_tip, path).await?;
    let theirs = argus_git::conflicts::blob_at(dir, &conflict.head_tip, path).await?;
    Ok(argus_git::conflicts::three_way_preview(
        scratch,
        &side(&conflict.upstream, ours),
        &side("merge-base", base),
        &side(&conflict.head, theirs),
    )
    .await?)
}

fn side(label: &str, content: Option<Vec<u8>>) -> argus_git::conflicts::PreviewSide {
    argus_git::conflicts::PreviewSide {
        label: label.to_owned(),
        content,
    }
}

/// A regular file's content (a symlink is not followed); `None` when it
/// is missing or not a regular file.
async fn read_regular(file: &Path) -> Option<Vec<u8>> {
    let meta = tokio::fs::symlink_metadata(file).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    tokio::fs::read(file).await.ok()
}

/// The runs of the pull-before-push resolutions in progress for a
/// repository, newest first, as run panel rows: they have no comment, so
/// [`argus_store::RunStore::list_recent_for_repo`] does not list them.
pub(crate) async fn listings(
    services: &Services,
    repo_id: argus_store::RepoId,
) -> Result<Vec<argus_store::RunListing>, argus_store::StoreError> {
    let sessions: Vec<(ResolveId, RebaseSession)> = {
        let rebases = services.resolves.lock();
        rebases
            .iter()
            .filter(|(_, session)| session.request.repo_id == repo_id)
            .map(|(id, session)| (*id, session.clone()))
            .collect()
    };
    let mut out = Vec::new();
    for (_, session) in sessions {
        let runs = services
            .store
            .runs()
            .find_by_worktree(session.worktree.path())
            .await?;
        for run in runs.into_iter().filter(|run| run.comment_id.is_none()) {
            let what = match run.kind {
                RunKind::Followup => "Reply on the resolution",
                _ => "Resolve",
            };
            out.push(argus_store::RunListing {
                file_path: format!(
                    "pull before push · {} ← {}",
                    session.request.head, session.upstream
                ),
                start_line: 0,
                end_line: 0,
                body_md: format!("{what}: {}", session.stop.subject),
                run,
            });
        }
    }
    out.sort_by_key(|listing| std::cmp::Reverse(listing.run.created_at));
    Ok(out)
}
