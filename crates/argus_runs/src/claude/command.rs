//! [`ClaudeCommand`]: the only place `claude` argument vectors are built.
//!
//! Review runs (spec §6.2) get the capabilities of a normal Claude Code
//! session: every tool, the user's settings, MCP servers, skills and
//! plugins, in `auto` permission mode. What the mode does not settle comes
//! back to Argus as a permission request (see [`crate::permission`]).
//! Commit messages follow spec §6.4. Arguments go straight to `execve`
//! through [`tokio::process::Command`]; no shell is involved, so prompts and
//! paths are never interpolated.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use uuid::Uuid;

use super::detect::{AUTO_MODE, ClaudeCapabilities, EFFORT_FLAG};
use super::env::{apply_clean_env, apply_session_env};
use crate::permission::user_message_line;

/// Permission mode of a review run when the CLI has no `auto` mode.
pub const FALLBACK_PERMISSION_MODE: &str = "acceptEdits";

/// Commands a review run may never run, whatever the permission mode or the
/// reviewer's answers: nothing is committed until Accept and only Push
/// sends commits (spec §11).
pub const REVIEW_DISALLOWED_TOOLS: [&str; 2] = ["Bash(git commit *)", "Bash(git push *)"];

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
        permission_mode: &'static str,
        /// The user's checkout, which the file tools may not touch.
        protected: Option<PathBuf>,
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
    /// claude -p --model <model> --effort <effort>
    ///   (--session-id | --resume) <uuid>
    ///   --input-format stream-json --output-format stream-json --verbose
    ///   --permission-mode auto --permission-prompt-tool stdio
    ///   --disallowedTools "Bash(git commit *)" "Bash(git push *)"
    ///     ["Edit(/<checkout>/**)" "Write(/<checkout>/**)"]
    ///   --append-system-prompt-file <rules_path>
    /// ```
    ///
    /// No tool allow list, `--max-turns` or `--max-budget-usd`: the run has
    /// every capability of a normal session and is bounded only by Stop. The
    /// prompt is the first user message on stdin ([`ClaudeCommand::stdin`]);
    /// stdin stays open for the answers to permission requests, and the
    /// run manager closes it after the `result` line. `effort: None` omits
    /// `--effort`.
    pub fn review_run(
        cwd: impl Into<PathBuf>,
        prompt: impl Into<String>,
        model: impl Into<String>,
        effort: Option<&str>,
        session: SessionMode,
        rules_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cwd: Some(cwd.into()),
            kind: Kind::Review {
                prompt: prompt.into(),
                permission_mode: AUTO_MODE,
                protected: None,
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
    ///   --strict-mcp-config "Write a git commit message for this change: <format rules>. Review comment: <text>"
    /// ```
    ///
    /// `--tools ""` only removes the built-in tools; `--strict-mcp-config`
    /// also drops the user's MCP servers, whose tools Haiku would otherwise
    /// call and so use up its one turn.
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
                    "Write a git commit message for this change: a subject line of at \
                     most 72 characters, a blank line, then the body. Describe the \
                     change itself; do not quote the review comment or mention it. \
                     Review comment, for context only: {comment}"
                ),
            },
        }
    }

    /// Denies the file-editing tools inside `checkout`, the user's working
    /// checkout of the reviewed repository (spec §11.1): a review run edits
    /// its own worktree only, whatever its permission mode allows.
    pub fn protecting(mut self, checkout: impl Into<PathBuf>) -> Self {
        if let Kind::Review { protected, .. } = &mut self.kind {
            *protected = Some(checkout.into());
        }
        self
    }

    /// Sets the working directory.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Fits the command to the installed CLI: drops `--effort` when it is
    /// unsupported, and falls back to [`FALLBACK_PERMISSION_MODE`] when the
    /// CLI has no `auto` mode.
    pub fn adapt_to(mut self, capabilities: &ClaudeCapabilities) -> Self {
        if let Kind::Review {
            effort,
            permission_mode,
            ..
        } = &mut self.kind
        {
            if !capabilities.supports(EFFORT_FLAG) {
                *effort = None;
            }
            if !capabilities.supports_auto_mode() {
                *permission_mode = FALLBACK_PERMISSION_MODE;
            }
        }
        self
    }

    /// Whether this is a review run, which talks `stream-json` on stdin and
    /// must have its stdin closed after the `result` line.
    pub fn is_review(&self) -> bool {
        matches!(self.kind, Kind::Review { .. })
    }

    /// The working directory, when set.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The arguments after the program name.
    pub fn args(&self) -> Vec<OsString> {
        match &self.kind {
            Kind::Review {
                model,
                effort,
                session,
                rules_path,
                permission_mode,
                protected,
                ..
            } => {
                let mut args: Vec<OsString> = vec!["-p".into()];
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
                        "--input-format",
                        "stream-json",
                        "--output-format",
                        "stream-json",
                        "--verbose",
                        "--permission-mode",
                        permission_mode,
                        "--permission-prompt-tool",
                        "stdio",
                        "--disallowedTools",
                    ]
                    .map(OsString::from),
                );
                args.extend(REVIEW_DISALLOWED_TOOLS.map(OsString::from));
                if let Some(checkout) = protected.as_deref().filter(|path| path.is_absolute()) {
                    // `//` starts an absolute path in a permission rule.
                    let pattern = format!("/{}/**", checkout.display());
                    args.extend(["Edit", "Write"].map(|tool| format!("{tool}({pattern})").into()));
                }
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
                    "--strict-mcp-config",
                ]
                .map(OsString::from)
                .into();
                args.push(prompt.into());
                args
            }
        }
    }

    /// The first bytes to write to the child's stdin: the staged diff of a
    /// commit message (then stdin is closed), or the prompt of a review run
    /// as a `stream-json` user message line.
    pub fn stdin(&self) -> Vec<u8> {
        match &self.kind {
            Kind::Review { prompt, .. } => {
                let mut line = user_message_line(prompt).into_bytes();
                line.push(b'\n');
                line
            }
            Kind::CommitMessage { diff, .. } => diff.clone(),
        }
    }

    /// The process to spawn with `program` (the detected `claude` binary).
    ///
    /// It has the working directory, the argv, its environment (see
    /// [`super::env`]: the user's own for a review run, a clean one for a
    /// commit message), piped stdin/stdout/stderr, and its own process group
    /// so Stop can signal the whole tree. The child is killed when its handle
    /// is dropped.
    pub fn to_command(&self, program: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(program);
        command.args(self.args());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if self.is_review() {
            apply_session_env(&mut command, program);
        } else {
            apply_clean_env(&mut command, program);
        }
        command
            .stdin(Stdio::piped())
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
