//! Write side of the git service, through the `git` CLI.
//!
//! Writes touch refs and config (branch creation, the compare-and-swap ref
//! update of Accept). The one exception is [`sync_checkout`], which carries a
//! checkout of the user's along when its branch moves, and never overwrites
//! a change of the user's (spec §11).

use crate::cli::GitCli;
use crate::error::{GitError, Result};
use std::path::Path;

use crate::read::{Repo, Sha, git};

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

/// The commit `refs/heads/<branch>` points to now, read with `git rev-parse`
/// straight from the ref store (no cached handle), as the expected old value
/// of [`update_branch`].
pub async fn branch_tip(repo: &Repo, branch: &str) -> Result<Sha> {
    let full = local_ref(branch)?;
    let spec = format!("{full}^{{commit}}");
    let output = GitCli::new(repo.workdir())
        .run_unchecked(["rev-parse", "--verify", "--quiet", spec.as_str()])
        .await?;
    if !output.success() {
        return Err(GitError::RefNotFound { name: full });
    }
    let hex = output.stdout_lossy();
    Sha::parse(hex.trim()).ok_or_else(|| GitError::Parse {
        command: format!("git rev-parse --verify {spec}"),
        message: format!("not an object id: `{}`", hex.trim()),
    })
}

/// How a compare-and-swap ref update went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefUpdate {
    /// The branch now points to the new commit.
    Moved,
    /// The branch no longer pointed to the expected commit (something else
    /// moved it); it was left alone.
    Stale {
        /// Where the branch points now; `None` when it was deleted.
        current: Option<Sha>,
    },
}

/// Moves `refs/heads/<branch>` from `expected` to `new` with a
/// compare-and-swap: `git update-ref -m <reason> refs/heads/<branch> <new>
/// <expected>` (spec §5.3, safety invariant 4).
///
/// Only the ref (and its reflog) changes: a checkout that has the branch
/// checked out keeps its files and index as they are until
/// [`sync_checkout`] carries it along.
pub async fn update_branch(
    repo: &Repo,
    branch: &str,
    new: &Sha,
    expected: &Sha,
    reason: &str,
) -> Result<RefUpdate> {
    let full = local_ref(branch)?;
    let (new_hex, old_hex) = (new.to_hex(), expected.to_hex());
    let output = GitCli::new(repo.workdir())
        .run_unchecked([
            "update-ref",
            "-m",
            reason,
            full.as_str(),
            new_hex.as_str(),
            old_hex.as_str(),
        ])
        .await?;
    if output.success() {
        tracing::info!(branch, %new, %expected, "moved branch");
        return Ok(RefUpdate::Moved);
    }
    // Tell a lost race apart from any other failure (permissions, a lock
    // held by another git process, ...).
    match branch_tip(repo, branch).await {
        Ok(current) if current != *expected => Ok(RefUpdate::Stale {
            current: Some(current),
        }),
        Err(GitError::RefNotFound { .. }) => Ok(RefUpdate::Stale { current: None }),
        _ => Err(GitError::Command {
            command: format!("git update-ref -m {reason} {full} {new_hex} {old_hex}"),
            code: output.code,
            stderr: output.stderr,
        }),
    }
}

/// How [`sync_checkout`] went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckoutSync {
    /// The checkout's index and files now match the new commit.
    Updated,
    /// The checkout was left as it was; `reason` says why (git's message,
    /// e.g. a file with changes of the user's that the new commit changes).
    Skipped {
        /// Why, for the user.
        reason: String,
    },
}

/// Carries the checkout at `path`, whose branch was just moved from `from`
/// to `to` by [`update_branch`], along to `to`: `git read-tree -m -u <from>
/// <to>`, what `git checkout` does when it switches commits.
///
/// Only the paths that differ between `from` and `to` are written. Changes
/// of the user's elsewhere are kept as they are; a staged or unstaged change
/// (or an untracked file) in one of those paths makes git refuse before
/// writing anything, and the checkout stays as it was. Does nothing when the
/// checkout's `HEAD` is no longer `to` (the user switched branches).
pub async fn sync_checkout(path: &Path, from: &Sha, to: &Sha) -> Result<CheckoutSync> {
    let cli = GitCli::new(path);
    let head = cli
        .run_unchecked(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await?;
    if !head.success() || Sha::parse(head.stdout_lossy().trim()) != Some(*to) {
        return Ok(CheckoutSync::Skipped {
            reason: "its HEAD is no longer the moved branch".to_owned(),
        });
    }
    let (from_hex, to_hex) = (from.to_hex(), to.to_hex());
    let output = cli
        .run_unchecked(["read-tree", "-m", "-u", from_hex.as_str(), to_hex.as_str()])
        .await?;
    if output.success() {
        tracing::info!(path = %path.display(), %from, %to, "updated the checkout");
        Ok(CheckoutSync::Updated)
    } else {
        tracing::info!(path = %path.display(), stderr = %output.stderr, "checkout left as is");
        Ok(CheckoutSync::Skipped {
            reason: output
                .stderr
                .lines()
                .next()
                .unwrap_or("git read-tree failed")
                .trim_start_matches("error: ")
                .to_owned(),
        })
    }
}

/// `refs/heads/<branch>`, refusing names git would misread.
fn local_ref(branch: &str) -> Result<String> {
    let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    if branch.is_empty() || branch.starts_with('-') || branch.contains(char::is_whitespace) {
        return Err(GitError::InvalidBranchName {
            name: branch.to_owned(),
        });
    }
    Ok(format!("refs/heads/{branch}"))
}
