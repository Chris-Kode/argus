//! Thin wrapper around the `git` CLI, used by every write (spec §5.2).
//!
//! Writes go through the real `git` binary so worktrees, the index, commits
//! and rebases behave exactly like git itself. Each command runs as
//! `git -C <dir> ...`, cannot prompt for credentials, prints untranslated
//! messages, and is logged at debug level.
//!
//! The wrapper is also where safety invariant 3 is enforced in code: every
//! argv passes [`check_args`] before git starts, and a `git push` that could
//! rewrite or delete remote history (`--force`, `--force-with-lease`, a
//! `+refspec`, `--delete`, `--mirror`, ...) is refused with
//! [`GitError::ForcePushRefused`]. The `--force` of `git worktree remove` and
//! `git checkout` only ever target Argus' own worktrees and stay allowed.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use tokio::io::AsyncWriteExt as _;
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
        let output = self.output(&args, None).await?;
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
        self.output(&args, None).await
    }

    /// Runs `git <args>` with `input` on its standard input (e.g. a patch
    /// for `git apply -` or a message for `git commit -F -`) and returns its
    /// output whatever the exit code; only a failure to start git is an
    /// error.
    pub async fn run_with_input<I, S>(&self, args: I, input: &[u8]) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        self.output(&args, Some(input)).await
    }

    async fn output(&self, args: &[OsString], input: Option<&[u8]>) -> Result<GitOutput> {
        check_args(args)?;
        let started = Instant::now();
        let spawn_error = |source| GitError::Spawn {
            command: command_line(args),
            source,
        };
        let mut command = self.command(args);
        let output = match input {
            None => command.output().await.map_err(spawn_error)?,
            Some(input) => {
                let mut child = command.stdin(Stdio::piped()).spawn().map_err(spawn_error)?;
                let mut stdin = child.stdin.take();
                let input = input.to_vec();
                // Feed stdin while the output is read, so a large input
                // cannot deadlock on a full pipe.
                let writer = async move {
                    if let Some(stdin) = stdin.as_mut() {
                        // git may exit before reading everything; its exit
                        // code reports why.
                        let _ = stdin.write_all(&input).await;
                    }
                    drop(stdin);
                };
                let (output, ()) = tokio::join!(child.wait_with_output(), writer);
                output.map_err(spawn_error)?
            }
        };
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

/// Global options of `git` that take their value as the next argument.
const GLOBAL_OPTIONS_WITH_VALUE: [&str; 6] = [
    "-c",
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
];

/// `git push` options that can overwrite or delete remote history.
const FORBIDDEN_PUSH_OPTIONS: [&str; 7] = [
    "--force",
    "--force-with-lease",
    "--force-if-includes",
    "--delete",
    "--mirror",
    "--prune",
    "--all",
];

/// Refuses an argv Argus must never run: a `git push` that forces, deletes
/// or mirrors (safety invariant 3). Every command of [`GitCli`] passes here
/// before git starts.
///
/// The subcommand is found after the global options (`-c key=value`, ...).
/// For `push`, the long options in [`FORBIDDEN_PUSH_OPTIONS`] (also as
/// `--option=value`), the short `-f` and `-d` (alone or in a cluster such
/// as `-uf`), and refspecs that force (`+src:dst`) or delete (`:dst`) are
/// refused. Arguments after `--` are refspecs too.
pub fn check_args<S: AsRef<OsStr>>(args: &[S]) -> Result<()> {
    let refused = || GitError::ForcePushRefused {
        command: command_line(
            &args
                .iter()
                .map(|a| a.as_ref().to_owned())
                .collect::<Vec<_>>(),
        ),
    };
    let mut rest = args.iter().map(|arg| arg.as_ref().to_string_lossy());
    let subcommand = loop {
        let Some(arg) = rest.next() else {
            return Ok(());
        };
        if GLOBAL_OPTIONS_WITH_VALUE.contains(&arg.as_ref()) {
            rest.next();
        } else if !arg.starts_with('-') {
            break arg;
        }
    };
    if subcommand != "push" {
        return Ok(());
    }
    let mut options_done = false;
    for arg in rest {
        if !options_done && arg == "--" {
            options_done = true;
            continue;
        }
        if !options_done && arg.starts_with("--") {
            let name = arg.split_once('=').map_or(arg.as_ref(), |(name, _)| name);
            if FORBIDDEN_PUSH_OPTIONS.contains(&name) {
                return Err(refused());
            }
            continue;
        }
        if !options_done && arg.starts_with('-') && arg.len() > 1 {
            if arg[1..].contains(['f', 'd']) {
                return Err(refused());
            }
            continue;
        }
        // A positional argument: the remote or a refspec.
        if arg.starts_with('+') || arg.starts_with(':') {
            return Err(refused());
        }
    }
    Ok(())
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
