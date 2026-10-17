//! [`ClaudeCommand`]: the only place `claude` argument vectors are built.
//!
//! The argv follows spec §6.2 (review runs) and §6.4 (commit messages)
//! exactly. Arguments go straight to `execve` through
//! [`tokio::process::Command`]; no shell is involved, so prompts and paths
//! are never interpolated.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use uuid::Uuid;

use super::detect::{ClaudeCapabilities, EFFORT_FLAG};
use super::env::apply_clean_env;

/// Prompts longer than this (in bytes) go on stdin instead of the argv: Linux
/// caps a single argument at 128 KiB (`MAX_ARG_STRLEN`).
pub const LARGE_PROMPT_BYTES: usize = 100 * 1024;

/// Tools a review run may use (spec §6.2, §15): reads, edits and `git diff`
/// only, so Claude runs no build, test or lint commands.
pub const REVIEW_ALLOWED_TOOLS: [&str; 4] = ["Read", "Edit", "Write", "Bash(git diff *)"];

/// Model of the commit-message call (spec §6.4).
pub const COMMIT_MESSAGE_MODEL: &str = "haiku";

/// Longest review comment quoted in the commit-message prompt, in bytes. The
/// prompt must stay an argument because the diff takes stdin.
const COMMIT_COMMENT_MAX_BYTES: usize = 8 * 1024;

/// How a review run maps to its Claude session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMode {
    /// A comment's first run: `--session-id <uuid>`; Argus picks the UUID.
    New(Uuid),
    /// A follow-up: `--resume <uuid>` continues the comment's session.
    Resume(Uuid),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Review {
        prompt: String,
        model: String,
        effort: Option<String>,
        session: SessionMode,
        rules_path: PathBuf,
        prompt_on_stdin: bool,
    },
    CommitMessage {
        diff: Vec<u8>,
        prompt: String,
    },
}

/// A `claude` invocation: working directory, argv and stdin.
///
/// Build it with [`ClaudeCommand::review_run`] or
/// [`ClaudeCommand::commit_message`], adapt it to the installed CLI with
/// [`ClaudeCommand::adapt_to`], then spawn [`ClaudeCommand::to_command`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeCommand {
    cwd: Option<PathBuf>,
    kind: Kind,
}

impl ClaudeCommand {
    /// A comment, follow-up or resolve run in `cwd` (the run's worktree):
    ///
    /// ```text
    /// claude -p <prompt> --model <model> --effort <effort>
    ///   (--session-id | --resume) <uuid>
    ///   --output-format stream-json --verbose
    ///   --permission-mode acceptEdits
    ///   --allowedTools Read Edit Write "Bash(git diff *)"
    ///   --permission-prompts none
    ///   --append-system-prompt-file <rules_path>
    /// ```
    ///
    /// No `--max-turns` or `--max-budget-usd`: runs are bounded only by Stop.
    /// A prompt over [`LARGE_PROMPT_BYTES`] is sent on stdin instead.
    /// `effort: None` omits `--effort`.
    pub fn review_run(
        cwd: impl Into<PathBuf>,
        prompt: impl Into<String>,
        model: impl Into<String>,
        effort: Option<&str>,
        session: SessionMode,
        rules_path: impl Into<PathBuf>,
    ) -> Self {
        let prompt = prompt.into();
        Self {
            cwd: Some(cwd.into()),
            kind: Kind::Review {
                prompt_on_stdin: prompt.len() > LARGE_PROMPT_BYTES,
                prompt,
                model: model.into(),
                effort: effort.map(str::to_owned),
                session,
                rules_path: rules_path.into(),
            },
        }
    }

    /// The Haiku commit-message call of an Accept (spec §6.4), the
    /// equivalent of:
    ///
    /// ```text
    /// git diff --cached | claude -p --model haiku --tools "" --max-turns 1 \
    ///   "Write a git commit message for this change. Review comment: <text>"
    /// ```
    ///
    /// `diff_stdin` is the staged diff. Very long comments are truncated in
    /// the prompt. The working directory is inherited unless set with
    /// [`ClaudeCommand::with_cwd`].
    pub fn commit_message(diff_stdin: impl Into<Vec<u8>>, comment_text: &str) -> Self {
        let comment = truncate_at_char(comment_text.trim(), COMMIT_COMMENT_MAX_BYTES);
        Self {
            cwd: None,
            kind: Kind::CommitMessage {
                diff: diff_stdin.into(),
                prompt: format!(
                    "Write a git commit message for this change. Review comment: {comment}"
                ),
            },
        }
    }

    /// Sets the working directory.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Fits the command to the installed CLI: drops `--effort` when it is
    /// unsupported, and keeps a long prompt in the argv when the CLI cannot
    /// read it from stdin.
    pub fn adapt_to(mut self, capabilities: &ClaudeCapabilities) -> Self {
        if let Kind::Review {
            effort,
            prompt_on_stdin,
            ..
        } = &mut self.kind
        {
            if !capabilities.supports(EFFORT_FLAG) {
                *effort = None;
            }
            if !capabilities.stdin_prompt() {
                *prompt_on_stdin = false;
            }
        }
        self
    }

    /// The working directory, when set.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The arguments after the program name.
    pub fn args(&self) -> Vec<OsString> {
        match &self.kind {
            Kind::Review {
                prompt,
                model,
                effort,
                session,
                rules_path,
                prompt_on_stdin,
            } => {
                let mut args: Vec<OsString> = vec!["-p".into()];
                if !prompt_on_stdin {
                    args.push(prompt.into());
                }
                args.extend(["--model".into(), model.into()]);
                if let Some(effort) = effort {
                    args.extend(["--effort".into(), effort.into()]);
                }
                let (flag, id) = match session {
                    SessionMode::New(id) => ("--session-id", id),
                    SessionMode::Resume(id) => ("--resume", id),
                };
                args.extend([flag.into(), id.hyphenated().to_string().into()]);
                args.extend(
                    [
                        "--output-format",
                        "stream-json",
                        "--verbose",
                        "--permission-mode",
                        "acceptEdits",
                        "--allowedTools",
                    ]
                    .map(OsString::from),
                );
                args.extend(REVIEW_ALLOWED_TOOLS.map(OsString::from));
                args.extend(["--permission-prompts", "none"].map(OsString::from));
                args.extend([
                    "--append-system-prompt-file".into(),
                    rules_path.as_os_str().to_owned(),
                ]);
                args
            }
            Kind::CommitMessage { prompt, .. } => {
                let mut args: Vec<OsString> = [
                    "-p",
                    "--model",
                    COMMIT_MESSAGE_MODEL,
                    "--tools",
                    "",
                    "--max-turns",
                    "1",
                ]
                .map(OsString::from)
                .into();
                args.push(prompt.into());
                args
            }
        }
    }

    /// Bytes to write to the child's stdin before closing it: the staged diff
    /// of a commit message, or a review prompt too long for the argv.
    pub fn stdin(&self) -> Option<&[u8]> {
        match &self.kind {
            Kind::Review {
                prompt,
                prompt_on_stdin: true,
                ..
            } => Some(prompt.as_bytes()),
            Kind::Review { .. } => None,
            Kind::CommitMessage { diff, .. } => Some(diff),
        }
    }

    /// The process to spawn with `program` (the detected `claude` binary).
    ///
    /// It has the working directory, the argv, a clean environment (see
    /// [`super::env`]), piped stdout/stderr, stdin piped when
    /// [`ClaudeCommand::stdin`] has bytes (null otherwise), and its own
    /// process group so Stop can signal the whole tree. The child is killed
    /// when its handle is dropped.
    pub fn to_command(&self, program: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(program);
        command.args(self.args());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        apply_clean_env(&mut command, program);
        command
            .stdin(if self.stdin().is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        command
    }
}

/// The longest prefix of `text` of at most `max` bytes, on a char boundary.
fn truncate_at_char(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
