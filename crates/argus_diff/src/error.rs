//! Typed errors of the diff engine.

use argus_git::GitError;

/// Result alias used across `argus_diff`.
pub type Result<T, E = DiffError> = std::result::Result<T, E>;

/// Everything that can go wrong while computing a diff.
#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    /// Reading refs, trees or blobs failed.
    #[error(transparent)]
    Git(GitError),

    /// The path is not part of this session's file list.
    #[error("`{path}` is not changed in this diff")]
    FileNotFound {
        /// The path as passed by the caller.
        path: String,
    },

    /// The job was cancelled: its future was dropped or the session ended.
    #[error("diff cancelled")]
    Cancelled,

    /// A background task died (panic) before returning a result.
    #[error("diff background task failed: {0}")]
    Task(String),
}

impl From<GitError> for DiffError {
    fn from(err: GitError) -> Self {
        match err {
            GitError::Cancelled => Self::Cancelled,
            err => Self::Git(err),
        }
    }
}
