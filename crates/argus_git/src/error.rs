//! Typed errors of the git service.

use std::path::PathBuf;

/// Result alias used across `argus_git`.
pub type Result<T, E = GitError> = std::result::Result<T, E>;

/// Everything that can go wrong in a git call.
///
/// Every message is meant to be shown to the user as-is: CLI failures carry
/// git's own stderr, `gix` failures carry the full cause chain.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// No git repository was found at the path or any of its parents.
    #[error("no git repository found at {path}: {message}")]
    NotARepository {
        /// Path the discovery started from.
        path: PathBuf,
        /// Reason reported by `gix`.
        message: String,
    },

    /// The repository has no working tree; Argus only reviews non-bare repos.
    #[error("{path} is a bare repository; open a repository with a working tree instead")]
    BareRepository {
        /// Git directory of the bare repository.
        path: PathBuf,
    },

    /// A branch, ref or commit name could not be resolved.
    #[error("cannot find a branch, ref or commit named `{name}`")]
    RefNotFound {
        /// The name as passed by the caller.
        name: String,
    },

    /// The name resolved to an object that does not peel to a commit.
    #[error("`{name}` does not point to a commit")]
    NotACommit {
        /// The name as passed by the caller.
        name: String,
    },

    /// The two branches share no common ancestor.
    #[error("`{base}` and `{head}` have unrelated histories (no merge-base)")]
    UnrelatedHistories {
        /// Base branch or commit.
        base: String,
        /// Head branch or commit.
        head: String,
    },

    /// A remote name that git would misinterpret (empty or option-like).
    #[error("invalid remote name `{name}`")]
    InvalidRemote {
        /// The rejected name.
        name: String,
    },

    /// A branch name that git would misinterpret (empty, option-like or
    /// containing whitespace).
    #[error("invalid branch name `{name}`")]
    InvalidBranchName {
        /// The rejected name.
        name: String,
    },

    /// A `gix` read failed.
    #[error("{context}: {message}")]
    Gix {
        /// What the service was doing.
        context: &'static str,
        /// Full `gix` error chain.
        message: String,
    },

    /// The `git` binary could not be started (missing from `PATH`, ...).
    #[error("could not run `{command}`: {source}")]
    Spawn {
        /// The command line, for display.
        command: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A `git` CLI command exited unsuccessfully.
    #[error("`{command}` failed{}: {stderr}", exit_suffix(*.code))]
    Command {
        /// The command line, for display.
        command: String,
        /// Exit code, `None` if killed by a signal.
        code: Option<i32>,
        /// Git's error output, progress lines removed.
        stderr: String,
    },

    /// A git command produced output that could not be parsed.
    #[error("unexpected output from `{command}`: {message}")]
    Parse {
        /// The command line, for display.
        command: String,
        /// What was wrong.
        message: String,
    },

    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A write targeted a path outside `~/.argus/worktrees/` (safety
    /// invariant 1: Argus only ever writes inside its own worktrees).
    #[error("refusing to write to {}: it is not inside the Argus worktrees root {}", path.display(), root.display())]
    OutsideWorktreesRoot {
        /// The rejected path.
        path: PathBuf,
        /// The worktrees root.
        root: PathBuf,
    },

    /// Reading or updating the runs table failed.
    #[error("storage error: {0}")]
    Store(#[from] argus_store::StoreError),

    /// The operation was cancelled by the caller.
    #[error("operation cancelled")]
    Cancelled,

    /// The background task died (panic) before returning a result.
    #[error("git background task failed: {0}")]
    Task(String),
}

fn exit_suffix(code: Option<i32>) -> String {
    match code {
        Some(code) => format!(" (exit code {code})"),
        None => " (killed by a signal)".to_owned(),
    }
}

impl GitError {
    /// Wraps a `gix` error with the operation it happened in.
    pub(crate) fn gix(context: &'static str, err: impl std::fmt::Display) -> Self {
        Self::Gix {
            context,
            message: format!("{err:#}"),
        }
    }
}
