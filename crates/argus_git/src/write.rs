//! Write side of the git service, through the `git` CLI.
//!
//! Every write here only touches refs and config: the user's working tree
//! and index are never read or modified (spec §11).

use crate::error::{GitError, Result};
use crate::read::{Repo, git};

/// Creates the local branch `name` at `remote_branch` (`origin/feature`) and
/// sets it as its upstream, like `git branch --track <name> <remote_branch>`.
///
/// Fails if `name` already exists; an existing branch is never moved.
pub async fn create_tracking_branch(repo: &Repo, name: &str, remote_branch: &str) -> Result<()> {
    for value in [name, remote_branch] {
        if value.is_empty() || value.starts_with('-') || value.contains(char::is_whitespace) {
            return Err(GitError::InvalidBranchName {
                name: value.to_owned(),
            });
        }
    }
    let display = format!("git branch --track {name} {remote_branch}");
    let output = git(repo)
        .args(["branch", "--track", name, remote_branch])
        .output()
        .await
        .map_err(|source| GitError::Spawn {
            command: display.clone(),
            source,
        })?;
    if output.status.success() {
        tracing::info!(name, remote_branch, "created tracking branch");
        Ok(())
    } else {
        Err(GitError::Command {
            command: display,
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}
