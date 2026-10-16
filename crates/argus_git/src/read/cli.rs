//! The two reads that use the `git` CLI: `git worktree list` and `git fetch`.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use super::{Repo, Sha};
use crate::error::{GitError, Result};

/// One working tree attached to the repository, from
/// `git worktree list --porcelain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// Root of the working tree.
    pub path: PathBuf,
    /// Short name of the checked-out branch, `None` when detached.
    pub branch: Option<String>,
    /// Commit at `HEAD`, `None` for an unborn branch.
    pub head: Option<Sha>,
}

/// Builds a `git` command bound to `repo`'s git directory, isolated from
/// git environment variables of the parent process and unable to prompt.
fn git(repo: &Repo) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(repo.git_dir())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        // Argus never asks for credentials; helpers configured by the user
        // still work, but git must not block on a terminal prompt.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Lists every working tree of the repository and the branch checked out in
/// each. Bare entries are skipped.
pub async fn current_checkouts(repo: &Repo) -> Result<Vec<Checkout>> {
    const DISPLAY: &str = "git worktree list --porcelain";
    let output = git(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .await
        .map_err(|source| GitError::Spawn {
            command: DISPLAY.to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(GitError::Command {
            command: DISPLAY.to_owned(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| GitError::Parse {
        command: DISPLAY.to_owned(),
        message: "output is not UTF-8".to_owned(),
    })?;
    Ok(parse_worktree_list(&stdout))
}

/// Parses porcelain records: blank-line separated blocks of
/// `worktree <path>`, `HEAD <sha>`, `branch <ref>` / `detached` / `bare`.
fn parse_worktree_list(stdout: &str) -> Vec<Checkout> {
    let mut checkouts = Vec::new();
    for block in stdout.split("\n\n") {
        let mut path = None;
        let mut head = None;
        let mut branch = None;
        let mut bare = false;
        for line in block.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => path = Some(PathBuf::from(value)),
                "HEAD" => head = Sha::parse(value),
                "branch" => {
                    branch = Some(
                        value
                            .strip_prefix("refs/heads/")
                            .unwrap_or(value)
                            .to_owned(),
                    )
                }
                "bare" => bare = true,
                _ => {}
            }
        }
        if let (Some(path), false) = (path, bare) {
            checkouts.push(Checkout { path, branch, head });
        }
    }
    checkouts
}

/// Runs `git fetch --prune <remote>` with the user's own git config and
/// credential helpers.
///
/// `on_progress` receives each progress or status line git prints, as it
/// arrives. On failure the error carries git's stderr (e.g. "Could not
/// resolve host", "Authentication failed") without the progress noise.
/// Dropping the future kills the git process.
pub async fn fetch(
    repo: &Repo,
    remote: &str,
    mut on_progress: impl FnMut(&str) + Send,
) -> Result<()> {
    if remote.is_empty() || remote.starts_with('-') || remote.contains(char::is_whitespace) {
        return Err(GitError::InvalidRemote {
            name: remote.to_owned(),
        });
    }
    let display = format!("git fetch --prune {remote}");
    let mut child = git(repo)
        .args(["fetch", "--prune", "--progress", remote])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| GitError::Spawn {
            command: display.clone(),
            source,
        })?;

    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| GitError::Task("git stderr not captured".to_owned()))?;
    let mut pending = Vec::new();
    let mut messages = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stderr
            .read(&mut chunk)
            .await
            .map_err(|source| GitError::Io {
                path: repo.git_dir().to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..read]);
        // Progress meters rewrite one line with `\r`; real messages end in `\n`.
        while let Some(end) = pending.iter().position(|&b| b == b'\r' || b == b'\n') {
            let terminator = pending[end];
            let line = String::from_utf8_lossy(&pending[..end])
                .trim_end()
                .to_owned();
            pending.drain(..=end);
            if line.is_empty() {
                continue;
            }
            on_progress(&line);
            if terminator == b'\n' {
                messages.push(line);
            }
        }
    }
    let tail = String::from_utf8_lossy(&pending).trim().to_owned();
    if !tail.is_empty() {
        on_progress(&tail);
        messages.push(tail);
    }

    let status = child.wait().await.map_err(|source| GitError::Spawn {
        command: display.clone(),
        source,
    })?;
    if status.success() {
        tracing::info!(remote, "fetch finished");
        Ok(())
    } else {
        Err(GitError::Command {
            command: display,
            code: status.code(),
            stderr: messages.join("\n"),
        })
    }
}
