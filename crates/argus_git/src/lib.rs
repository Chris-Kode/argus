//! Git access for Argus: `gix` for reads and diffs, the `git` CLI for writes.
//!
//! The read side lives in [`read`] (open repo, list branches, merge-base,
//! ahead/behind, checkouts, fetch) and [`objects`] (tree diffs with rename
//! detection and blob reads for the diff engine). The write side lives in
//! [`write`]; for now it only creates tracking branches; worktrees, commit,
//! rebase and push arrive with issue #17. Nothing here ever reads or modifies
//! a user's working checkout.

mod cancel;
mod error;
pub mod objects;
pub mod read;
pub mod write;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

pub use cancel::{CancelGuard, CancelToken};
pub use error::{GitError, Result};
pub use read::{Branch, BranchKind, Checkout, Repo, RepoId, Sha};

/// Handle to the git service.
///
/// Cheap to clone. It caches one opened [`Repo`] (and so one
/// `gix::ThreadSafeRepository` with its loaded pack indices) per git
/// directory, so each linked worktree keeps its own handle; the other reads are the free functions of [`read`].
#[derive(Clone, Debug, Default)]
pub struct GitService {
    repos: Arc<Mutex<HashMap<PathBuf, Repo>>>,
}

impl GitService {
    /// Creates the service handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the repository containing `path` (see [`read::open`]), reusing
    /// the cached handle when the repository was opened before.
    pub async fn open(&self, path: impl Into<PathBuf>) -> Result<Repo> {
        let repo = read::open(path).await?;
        let mut repos = self.repos.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(repos
            .entry(repo.git_dir().to_owned())
            .or_insert(repo)
            .clone())
    }

    /// Re-opens `repo` and replaces its cached handle, so config written by
    /// the CLI since it was opened (e.g. a new branch's upstream) is seen.
    pub async fn reload(&self, repo: &Repo) -> Result<Repo> {
        let fresh = read::open(repo.workdir().to_owned()).await?;
        self.repos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fresh.git_dir().to_owned(), fresh.clone());
        Ok(fresh)
    }

    /// Drops every cached handle of a repository, e.g. when it is closed.
    pub fn forget(&self, id: &RepoId) {
        self.repos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, repo| repo.id() != id);
    }
}
