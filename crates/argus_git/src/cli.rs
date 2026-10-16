//! Thin wrapper around the `git` CLI, used by every write (spec §5.2).
//!
//! Writes go through the real `git` binary so worktrees, the index, commits
//! and rebases behave exactly like git itself. Each command runs as
//! `git -C <dir> ...`, cannot prompt for credentials, prints untranslated
//! messages, and is logged at debug level.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use tokio::process::Command;

use crate::error::{GitError, Result};

/// What a finished `git` command printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitOutput {
    /// Raw standard output (may be binary, e.g. a `--binary` patch).
    pub stdout: Vec<u8>,
    /// Standard error, lossily decoded and trimmed.
    pub stderr: String,
    /// Exit code, `None` if git was killed by a signal.
    pub code: Option<i32>,
}

impl GitOutput {
    /// Whether git exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Standard output as text, lossily decoded.
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Runs `git -C <dir> ...` with a fixed, non-interactive environment.
///
/// Cheap to clone. Dropping a returned future kills the git process, so every
/// command is cancellable.
#[derive(Clone, Debug)]
pub struct GitCli {
    dir: PathBuf,
}

impl GitCli {
    /// A wrapper whose commands run in `dir` (`git -C <dir>`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Directory the commands run in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs `git <args>` and fails with [`GitError::Command`] (carrying
    /// git's stderr) on a non-zero exit.
    pub async fn run<I, S>(&self, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let output = self.output(&args).await?;
        if output.success() {
            Ok(output)
        } else {
            Err(GitError::Command {
                command: command_line(&args),
                code: output.code,
                stderr: output.stderr,
            })
        }
    }

    /// Runs `git <args>` and returns its output whatever the exit code; only
    /// a failure to start git is an error.
    pub async fn run_unchecked<I, S>(&self, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        self.output(&args).await
    }

    async fn output(&self, args: &[OsString]) -> Result<GitOutput> {
        let started = Instant::now();
        let output = self
            .command(args)
            .output()
            .await
            .map_err(|source| GitError::Spawn {
                command: command_line(args),
                source,
            })?;
        let output = GitOutput {
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            code: output.status.code(),
        };
        tracing::debug!(
            dir = %self.dir.display(),
            command = %command_line(args),
            code = ?output.code,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "git command finished"
        );
        Ok(output)
    }

    fn command(&self, args: &[OsString]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            // `-C` decides the repository; inherited overrides must not.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            // Never block on a terminal prompt; credential helpers still work.
            .env("GIT_TERMINAL_PROMPT", "0")
            // Untranslated messages, so errors read the same everywhere.
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

/// `git a b c`, for logs and error messages.
fn command_line(args: &[OsString]) -> String {
    let mut text = String::from("git");
    for arg in args {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text
}
