//! Read side of the git service (spec §4, §5.1).
//!
//! Repository, ref and commit reads go through `gix` in-process; only
//! `git worktree list` and `git fetch` shell out to the CLI. Nothing here
//! ever reads the working tree or the index of the user's checkout
//! (spec §11): everything is resolved from refs and objects.
//!
//! Every function is `async`. `gix` work runs on tokio's blocking pool and
//! CLI work on tokio processes, so callers (the UI) are never blocked.
//! Cancellation is "drop the future": long walks notice it and stop early,
//! and child processes are killed.

mod branches;
mod cli;
mod refs;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gix::ObjectId;

use crate::error::{GitError, Result};

pub use branches::{Branch, BranchKind, branches};
pub(crate) use cli::git;
pub use cli::{Checkout, current_checkouts, fetch};
pub use refs::{current_branch, remote_default_branch, remote_names, split_remote_branch};

/// Object cache given to every thread-local `gix` handle; speeds up repeated
/// commit lookups during branch listing and history walks.
const OBJECT_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Stable identifier of a repository: hex SHA-1 of the canonical path of its
/// common git directory.
///
/// All linked worktrees of one repository share the same id, and the id does
/// not change between runs as long as the repository is not moved.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoId(String);

impl RepoId {
    /// The id as lowercase hex.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A full commit (object) id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha(ObjectId);

impl Sha {
    /// Parses a full hex object id.
    pub fn parse(hex: &str) -> Option<Self> {
        ObjectId::from_hex(hex.as_bytes()).ok().map(Self)
    }

    /// Full lowercase hex form.
    pub fn to_hex(&self) -> String {
        self.0.to_string()
    }

    /// Abbreviated hex form (`len` characters, clamped to the full length).
    pub fn short(&self, len: usize) -> String {
        let hex = self.to_hex();
        hex[..len.min(hex.len())].to_owned()
    }
}

impl Sha {
    pub(crate) fn object_id(&self) -> ObjectId {
        self.0
    }
}

impl From<ObjectId> for Sha {
    fn from(id: ObjectId) -> Self {
        Self(id)
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// An opened, non-bare repository.
///
/// Cheap to clone: the cached `gix::ThreadSafeRepository` (with its loaded
/// pack indices) is shared between clones.
#[derive(Clone)]
pub struct Repo {
    inner: Arc<RepoInner>,
}

struct RepoInner {
    id: RepoId,
    git_dir: PathBuf,
    common_dir: PathBuf,
    workdir: PathBuf,
    gix: gix::ThreadSafeRepository,
}

impl fmt::Debug for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Repo")
            .field("id", &self.inner.id)
            .field("workdir", &self.inner.workdir)
            .field("git_dir", &self.inner.git_dir)
            .finish_non_exhaustive()
    }
}

impl Repo {
    /// Stable repository id.
    pub fn id(&self) -> &RepoId {
        &self.inner.id
    }

    /// Root of the working tree the repository was discovered from.
    pub fn workdir(&self) -> &Path {
        &self.inner.workdir
    }

    /// Git directory of that working tree (`.git`, or
    /// `.git/worktrees/<name>` for a linked worktree).
    pub fn git_dir(&self) -> &Path {
        &self.inner.git_dir
    }

    /// Canonical common git directory, shared by all linked worktrees.
    pub fn common_dir(&self) -> &Path {
        &self.inner.common_dir
    }

    /// A thread-local `gix` handle with an object cache. Call only from
    /// blocking-pool threads.
    pub(crate) fn local(&self) -> gix::Repository {
        let mut repo = self.inner.gix.to_thread_local();
        repo.object_cache_size_if_unset(OBJECT_CACHE_BYTES);
        repo
    }
}

/// Cancellation flag shared with a blocking task.
#[derive(Clone, Default)]
pub(crate) struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub(crate) fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) {
            Err(GitError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Raises the flag when dropped, i.e. when the awaiting future goes away.
struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.0.store(true, Ordering::Relaxed);
    }
}

/// Runs `work` on tokio's blocking pool. Dropping the returned future raises
/// the [`Cancel`] flag handed to `work`, which stops it at its next check.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(Cancel) -> Result<T> + Send + 'static,
{
    let cancel = Cancel::default();
    let _guard = CancelOnDrop(cancel.clone());
    match tokio::task::spawn_blocking(move || work(cancel)).await {
        Ok(result) => result,
        Err(err) if err.is_cancelled() => Err(GitError::Cancelled),
        Err(err) => Err(GitError::Task(err.to_string())),
    }
}

/// Opens the repository containing `path` (which may be a subdirectory).
///
/// Bare repositories are rejected with [`GitError::BareRepository`]. Only the
/// repository's config and paths are read; the index and working tree are
/// left alone.
pub async fn open(path: impl Into<PathBuf>) -> Result<Repo> {
    let path = path.into();
    blocking(move |_| open_blocking(&path)).await
}

fn open_blocking(path: &Path) -> Result<Repo> {
    let gix =
        gix::ThreadSafeRepository::discover(path).map_err(|err| GitError::NotARepository {
            path: path.to_owned(),
            message: format!("{err:#}"),
        })?;
    let local = gix.to_thread_local();
    let git_dir = local.git_dir().to_owned();
    let Some(workdir) = local.workdir().map(Path::to_owned) else {
        return Err(GitError::BareRepository { path: git_dir });
    };
    let common_dir = std::fs::canonicalize(local.common_dir()).map_err(|source| GitError::Io {
        path: local.common_dir().to_owned(),
        source,
    })?;
    drop(local);
    let id = repo_id(&common_dir)?;
    tracing::debug!(%id, workdir = %workdir.display(), "opened repository");
    Ok(Repo {
        inner: Arc::new(RepoInner {
            id,
            git_dir,
            common_dir,
            workdir,
            gix,
        }),
    })
}

fn repo_id(common_dir: &Path) -> Result<RepoId> {
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    let bytes = gix::path::into_bstr(common_dir)
        .map_err(|err| GitError::gix("encoding the repository path", err))?;
    hasher.update(&bytes);
    let digest = hasher
        .try_finalize()
        .map_err(|err| GitError::gix("hashing the repository path", err))?;
    Ok(RepoId(digest.to_string()))
}

/// Resolves a branch name (`main`, `origin/main`, `refs/heads/main`) or a
/// full hex SHA to a commit id.
pub(crate) fn resolve_commit(repo: &gix::Repository, name: &str) -> Result<ObjectId> {
    let not_found = || GitError::RefNotFound {
        name: name.to_owned(),
    };
    let reference = repo
        .try_find_reference(name)
        .map_err(|err| GitError::gix("looking up a reference", err))?;
    let id = match reference {
        Some(mut reference) => {
            reference
                .peel_to_commit()
                .map_err(|_| GitError::NotACommit {
                    name: name.to_owned(),
                })?
                .id
        }
        None => {
            let id = ObjectId::from_hex(name.as_bytes()).map_err(|_| not_found())?;
            let object = repo
                .try_find_object(id)
                .map_err(|err| GitError::gix("reading an object", err))?;
            let object = object.ok_or_else(not_found)?;
            object
                .peel_to_commit()
                .map_err(|_| GitError::NotACommit {
                    name: name.to_owned(),
                })?
                .id
        }
    };
    Ok(id)
}

/// Best common ancestor of `base` and `head` (branch names or SHAs), as used
/// for the three-dot diff.
///
/// Unrelated histories are reported as [`GitError::UnrelatedHistories`] so
/// the UI can show them directly.
pub async fn merge_base(repo: &Repo, base: &str, head: &str) -> Result<Sha> {
    let repo = repo.clone();
    let (base, head) = (base.to_owned(), head.to_owned());
    blocking(move |cancel| {
        let local = repo.local();
        let base_id = resolve_commit(&local, &base)?;
        let head_id = resolve_commit(&local, &head)?;
        cancel.check()?;
        let found = local
            .merge_base(base_id, head_id)
            .map_err(|err| GitError::gix("computing the merge-base", err))?;
        match found {
            Some(id) => Ok(Sha(id.detach())),
            None => Err(GitError::UnrelatedHistories { base, head }),
        }
    })
    .await
}

/// Number of commits `local` has that `upstream` lacks (ahead) and the
/// reverse (behind), like `git rev-list --left-right --count`.
pub async fn ahead_behind(repo: &Repo, local: &str, upstream: &str) -> Result<(u32, u32)> {
    let repo = repo.clone();
    let (local_name, upstream_name) = (local.to_owned(), upstream.to_owned());
    blocking(move |cancel| {
        let repo = repo.local();
        let local_id = resolve_commit(&repo, &local_name)?;
        let upstream_id = resolve_commit(&repo, &upstream_name)?;
        if local_id == upstream_id {
            return Ok((0, 0));
        }
        let ahead = count_exclusive(&repo, local_id, upstream_id, &cancel)?;
        let behind = count_exclusive(&repo, upstream_id, local_id, &cancel)?;
        Ok((ahead, behind))
    })
    .await
}

/// Counts commits reachable from `tip` but not from `hidden`.
fn count_exclusive(
    repo: &gix::Repository,
    tip: ObjectId,
    hidden: ObjectId,
    cancel: &Cancel,
) -> Result<u32> {
    let walk = repo
        .rev_walk([tip])
        .with_hidden([hidden])
        .all()
        .map_err(|err| GitError::gix("walking history", err))?;
    let mut count = 0u32;
    for info in walk {
        info.map_err(|err| GitError::gix("walking history", err))?;
        count = count.saturating_add(1);
        if count.is_multiple_of(1024) {
            cancel.check()?;
        }
    }
    Ok(count)
}
