//! Run and integration worktrees under `~/.argus/worktrees/` (spec §5.2, §11).
//!
//! Every Claude run edits its own detached worktree at
//! `<root>/<repo-id>/<run-id>`; Accept and Push share one `_integration`
//! worktree per repository. The user's checkout is never written: the only
//! commands run against the repository itself are `git worktree
//! add/remove/prune/list`, which touch `.git/worktrees/` and nothing else.
//!
//! [`WorktreeManager::guard`] is the code-level guarantee of safety invariant 1:
//! every function that writes to a working tree first checks that the target
//! is strictly inside the worktrees root, after resolving symlinks.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use argus_store::{CommentStatus, RunId, RunOutcome, RunStatus, StorageService};

use crate::cli::GitCli;
use crate::error::{GitError, Result};
use crate::read::{Repo, RepoId, Sha, current_checkouts};

/// Name of the per-repository worktree used by Accept and Push.
pub const INTEGRATION_DIR: &str = "_integration";

/// `error` stored on runs that were active when Argus last exited.
pub const INTERRUPTED: &str = "interrupted";

/// A worktree owned by Argus. Only [`WorktreeManager`] hands these out, so the
/// path has passed the guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    path: PathBuf,
    repo_dir: PathBuf,
}

impl Worktree {
    /// Root of the worktree, below the worktrees root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Working directory of the repository the worktree belongs to.
    pub fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }
}

/// How a path differs between a run's start commit and its worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// New file.
    Added,
    /// Content or mode changed.
    Modified,
    /// File removed.
    Deleted,
    /// Type changed (file ↔ symlink ↔ submodule).
    TypeChanged,
}

/// One changed path in a [`Patch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    /// Path relative to the repository root.
    pub path: PathBuf,
    /// What happened to it.
    pub kind: ChangeKind,
}

/// Everything a run changed relative to its start commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    /// `git diff --cached --binary` output; applies with `git apply`.
    pub bytes: Vec<u8>,
    /// Changed paths, in git's order.
    pub files: Vec<ChangedFile>,
}

impl Patch {
    /// Whether the run changed nothing.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// What [`WorktreeManager::reconcile`] did for one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Orphan worktrees that were deleted.
    pub removed: Vec<PathBuf>,
    /// Worktrees kept because their run is active or holds a proposal.
    pub kept: Vec<PathBuf>,
    /// Orphans that could not be deleted, with the reason.
    pub failed: Vec<(PathBuf, String)>,
}

/// Creates, diffs, resets and removes Argus worktrees.
///
/// Cheap to clone.
#[derive(Clone, Debug)]
pub struct WorktreeManager {
    /// Canonical `<argus_home>/worktrees`.
    root: PathBuf,
    /// When the manager was opened; reconciliation leaves younger worktrees
    /// alone, since they belong to runs of this session.
    opened_at: SystemTime,
}

impl WorktreeManager {
    /// Opens (creating it if needed) the worktrees root
    /// `<argus_home>/worktrees`. The caller resolves `argus_home`, honoring
    /// `ARGUS_HOME`.
    pub async fn open(argus_home: impl Into<PathBuf>) -> Result<Self> {
        let root = argus_home.into().join("worktrees");
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|source| io_error(&root, source))?;
        let root = tokio::fs::canonicalize(&root)
            .await
            .map_err(|source| io_error(&root, source))?;
        tracing::debug!(root = %root.display(), "opened worktrees root");
        Ok(Self {
            root,
            opened_at: SystemTime::now(),
        })
    }

    /// Canonical worktrees root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding every worktree of one repository.
    pub fn repo_root(&self, repo: &RepoId) -> PathBuf {
        self.root.join(repo.as_str())
    }

    /// Path of a run's worktree.
    pub fn run_path(&self, repo: &RepoId, run_id: RunId) -> PathBuf {
        self.repo_root(repo).join(run_id.to_string())
    }

    /// Fails with [`GitError::OutsideWorktreesRoot`] unless `path` is an
    /// absolute path strictly inside the worktrees root.
    ///
    /// `..` components are rejected outright, and the deepest existing part
    /// of the path is resolved through symlinks, so a link pointing back into
    /// a user's checkout is refused too.
    pub fn guard(&self, path: &Path) -> Result<()> {
        let outside = || GitError::OutsideWorktreesRoot {
            path: path.to_owned(),
            root: self.root.clone(),
        };
        let lexical_ok = path.is_absolute()
            && path.components().all(|c| {
                matches!(
                    c,
                    Component::Normal(_) | Component::RootDir | Component::Prefix(_)
                )
            })
            && path
                .strip_prefix(&self.root)
                .is_ok_and(|rest| !rest.as_os_str().is_empty());
        if !lexical_ok {
            return Err(outside());
        }
        let mut existing = path;
        while existing.symlink_metadata().is_err() {
            existing = existing.parent().ok_or_else(outside)?;
        }
        let resolved = std::fs::canonicalize(existing).map_err(|_| outside())?;
        let inside = if existing == path {
            resolved.starts_with(&self.root) && resolved != self.root
        } else {
            resolved.starts_with(&self.root)
        };
        if inside { Ok(()) } else { Err(outside()) }
    }

    /// Wraps an existing path (e.g. a run's stored `worktree_path`) as a
    /// [`Worktree`] of `repo`, after the guard.
    pub fn worktree_at(&self, repo: &Repo, path: impl Into<PathBuf>) -> Result<Worktree> {
        let path = path.into();
        self.guard(&path)?;
        Ok(Worktree {
            path,
            repo_dir: repo.workdir().to_owned(),
        })
    }

    /// `git worktree add --detach <root>/<repo-id>/<run-id> <head-sha>`.
    ///
    /// Detached, so it works while the head branch is checked out in the
    /// user's checkout or anywhere else.
    pub async fn create_run_worktree(
        &self,
        repo: &Repo,
        head_sha: &Sha,
        run_id: RunId,
    ) -> Result<Worktree> {
        let path = self.run_path(repo.id(), run_id);
        self.add_detached(repo, &path, head_sha).await?;
        tracing::info!(path = %path.display(), %head_sha, "created run worktree");
        Ok(Worktree {
            path,
            repo_dir: repo.workdir().to_owned(),
        })
    }

    /// Stages everything in the worktree (`git add -A`, so new and deleted
    /// files count) and returns `git diff --cached --binary <start_sha>` with
    /// the changed paths.
    pub async fn diff_against_start(&self, worktree: &Worktree, start_sha: &Sha) -> Result<Patch> {
        self.guard(&worktree.path)?;
        let cli = GitCli::new(&worktree.path);
        cli.run(["add", "-A"]).await?;
        let start = start_sha.to_hex();
        // Pin every knob that changes the patch format, whatever the user's
        // config says, so the patch always applies with `git apply`.
        const FORMAT: [&str; 7] = [
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ];
        let mut patch_args = vec!["diff", "--cached", "--binary"];
        patch_args.extend(FORMAT);
        patch_args.extend([start.as_str(), "--"]);
        let bytes = cli.run(&patch_args).await?.stdout;

        let mut names_args = vec!["diff", "--cached", "--name-status", "-z"];
        names_args.extend(FORMAT);
        names_args.extend([start.as_str(), "--"]);
        let names = cli.run(&names_args).await?.stdout;
        let files = parse_name_status(&names).map_err(|message| GitError::Parse {
            command: format!("git {}", names_args.join(" ")),
            message,
        })?;
        Ok(Patch { bytes, files })
    }

    /// Stages everything in the worktree (`git add -A`) and writes the
    /// staged state as a tree object (`git write-tree`), so the proposal can
    /// be diffed like any two commits: `start_sha → tree`.
    ///
    /// Only the worktree's own index changes; the objects land in the
    /// repository's object store, as `git add` alone already does.
    pub async fn snapshot_tree(&self, worktree: &Worktree) -> Result<Sha> {
        self.guard(&worktree.path)?;
        let cli = GitCli::new(&worktree.path);
        cli.run(["add", "-A"]).await?;
        let output = cli.run(["write-tree"]).await?;
        let hex = output.stdout_lossy();
        Sha::parse(hex.trim()).ok_or_else(|| GitError::Parse {
            command: "git write-tree".to_owned(),
            message: format!("not an object id: `{}`", hex.trim()),
        })
    }

    /// `git worktree remove --force <path>` then `git worktree prune`.
    ///
    /// A directory git does not know as a worktree (a leftover of a crash)
    /// is deleted directly, still behind the guard.
    pub async fn remove(&self, worktree: &Worktree) -> Result<()> {
        self.guard(&worktree.path)?;
        let repo = GitCli::new(&worktree.repo_dir);
        let output = repo
            .run_unchecked([
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                worktree.path.as_os_str(),
            ])
            .await?;
        if !output.success() && worktree.path.symlink_metadata().is_ok() {
            tracing::debug!(
                path = %worktree.path.display(),
                stderr = %output.stderr,
                "git worktree remove failed; deleting the directory"
            );
            self.delete_dir(&worktree.path).await?;
        }
        repo.run(["worktree", "prune"]).await?;
        tracing::info!(path = %worktree.path.display(), "removed worktree");
        Ok(())
    }

    /// The repository's `_integration` worktree, created if missing and reset
    /// to `sha`: `git checkout --detach`, `git reset --hard`, `git clean
    /// -fdx`, all inside that worktree only.
    pub async fn integration_worktree(&self, repo: &Repo, sha: &Sha) -> Result<Worktree> {
        let path = self.repo_root(repo.id()).join(INTEGRATION_DIR);
        self.guard(&path)?;
        let worktree = Worktree {
            path,
            repo_dir: repo.workdir().to_owned(),
        };
        if self.is_registered(repo, &worktree.path).await? && worktree.path.is_dir() {
            self.reset(&worktree, sha).await?;
        } else {
            if worktree.path.symlink_metadata().is_ok() {
                self.delete_dir(&worktree.path).await?;
            }
            GitCli::new(repo.workdir())
                .run(["worktree", "prune"])
                .await?;
            self.add_detached(repo, &worktree.path, sha).await?;
            tracing::info!(path = %worktree.path.display(), "created integration worktree");
        }
        Ok(worktree)
    }

    /// Resets a worktree to `sha`, dropping every local change and untracked
    /// or ignored file in it.
    pub async fn reset(&self, worktree: &Worktree, sha: &Sha) -> Result<()> {
        self.guard(&worktree.path)?;
        let cli = GitCli::new(&worktree.path);
        // A crash mid-rebase would leave state that blocks the checkout; `--quit`
        // drops it without touching HEAD and is a no-op otherwise.
        cli.run_unchecked(["rebase", "--quit"]).await?;
        let sha = sha.to_hex();
        cli.run(["checkout", "--force", "--detach", sha.as_str()])
            .await?;
        cli.run(["reset", "--hard", sha.as_str()]).await?;
        cli.run(["clean", "-ffdx"]).await?;
        tracing::debug!(path = %worktree.path.display(), %sha, "reset worktree");
        Ok(())
    }

    /// Marks every run left `Queued` or `Running` by a previous session as
    /// `Failed` with error [`INTERRUPTED`]. Call once at startup, before any
    /// run of the new session starts.
    pub async fn mark_interrupted_runs(store: &StorageService) -> Result<Vec<RunId>> {
        let runs = store.runs();
        let mut marked = Vec::new();
        for run in runs.list_active().await? {
            let mut outcome = RunOutcome::new(RunStatus::Failed);
            outcome.error = Some(INTERRUPTED.to_owned());
            runs.finish(run.id, outcome).await?;
            tracing::info!(run = %run.id, "marked interrupted run as failed");
            marked.push(run.id);
        }
        Ok(marked)
    }

    /// Startup reconciliation for one repository.
    ///
    /// Compares the worktrees under `<root>/<repo-id>/` (on disk and in
    /// `git worktree list`) with the `runs` table and deletes orphans: no
    /// run row, or no run that is still active or holds a pending proposal
    /// (`Succeeded` with its comment neither resolved nor dismissed). The
    /// `_integration` worktree and worktrees created after the manager was
    /// opened are left alone. Run [`Self::mark_interrupted_runs`] first.
    pub async fn reconcile(&self, repo: &Repo, store: &StorageService) -> Result<ReconcileReport> {
        let repo_root = self.repo_root(repo.id());
        let mut candidates = BTreeSet::new();
        for checkout in current_checkouts(repo).await? {
            let path = std::fs::canonicalize(&checkout.path).unwrap_or(checkout.path);
            if path.parent() == Some(repo_root.as_path()) {
                candidates.insert(path);
            }
        }
        match tokio::fs::read_dir(&repo_root).await {
            Ok(mut entries) => {
                while let Some(entry) = entries
                    .next_entry()
                    .await
                    .map_err(|source| io_error(&repo_root, source))?
                {
                    candidates.insert(entry.path());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&repo_root, source)),
        }

        let mut report = ReconcileReport::default();
        for path in candidates {
            if path.file_name() == Some(OsStr::new(INTEGRATION_DIR)) {
                continue;
            }
            let fresh = std::fs::symlink_metadata(&path)
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| modified >= self.opened_at);
            if fresh {
                continue;
            }
            if self.has_live_run(store, &path).await? {
                report.kept.push(path);
                continue;
            }
            let worktree = Worktree {
                path: path.clone(),
                repo_dir: repo.workdir().to_owned(),
            };
            match self.remove(&worktree).await {
                Ok(()) => report.removed.push(path),
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "could not remove orphan worktree");
                    report.failed.push((path, err.to_string()));
                }
            }
        }
        GitCli::new(repo.workdir())
            .run(["worktree", "prune"])
            .await?;
        tracing::info!(
            repo = %repo.id(),
            removed = report.removed.len(),
            kept = report.kept.len(),
            failed = report.failed.len(),
            "reconciled worktrees"
        );
        Ok(report)
    }

    /// Whether some run stored for `path` still needs its worktree.
    async fn has_live_run(&self, store: &StorageService, path: &Path) -> Result<bool> {
        for run in store.runs().find_by_worktree(path).await? {
            if run.status.is_active() {
                return Ok(true);
            }
            if run.status != RunStatus::Succeeded {
                continue;
            }
            let Some(comment_id) = run.comment_id else {
                return Ok(true);
            };
            let closed = store.comments().get(comment_id).await?.is_some_and(|c| {
                matches!(c.status, CommentStatus::Resolved | CommentStatus::Dismissed)
            });
            if !closed {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn is_registered(&self, repo: &Repo, path: &Path) -> Result<bool> {
        Ok(current_checkouts(repo).await?.iter().any(|checkout| {
            checkout.path == path
                || std::fs::canonicalize(&checkout.path).is_ok_and(|real| real == path)
        }))
    }

    async fn add_detached(&self, repo: &Repo, path: &Path, sha: &Sha) -> Result<()> {
        self.guard(path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| io_error(parent, source))?;
        }
        let sha = sha.to_hex();
        GitCli::new(repo.workdir())
            .run([
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                path.as_os_str(),
                OsStr::new(&sha),
            ])
            .await?;
        Ok(())
    }

    async fn delete_dir(&self, path: &Path) -> Result<()> {
        self.guard(path)?;
        let meta = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|source| io_error(path, source))?;
        let result = if meta.is_dir() {
            tokio::fs::remove_dir_all(path).await
        } else {
            tokio::fs::remove_file(path).await
        };
        result.map_err(|source| io_error(path, source))
    }
}

fn io_error(path: &Path, source: std::io::Error) -> GitError {
    GitError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Parses `git diff --name-status -z --no-renames`: `<status>\0<path>\0` pairs.
fn parse_name_status(bytes: &[u8]) -> std::result::Result<Vec<ChangedFile>, String> {
    let mut fields = bytes.split(|&b| b == 0).filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let Some(status) = fields.next() {
        let path = fields
            .next()
            .ok_or_else(|| "status without a path".to_owned())?;
        let kind = match status.first() {
            Some(b'A') => ChangeKind::Added,
            Some(b'M') => ChangeKind::Modified,
            Some(b'D') => ChangeKind::Deleted,
            Some(b'T') => ChangeKind::TypeChanged,
            _ => {
                return Err(format!(
                    "unknown status `{}`",
                    String::from_utf8_lossy(status)
                ));
            }
        };
        let path = gix::path::from_byte_slice(path)
            .map_err(|err| format!("invalid path: {err}"))?
            .to_owned();
        files.push(ChangedFile { path, kind });
    }
    Ok(files)
}
