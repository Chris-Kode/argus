//! The `claude` CLI: finding it, checking what the installed version supports,
//! and building the exact command lines Argus runs (spec §6.2, §6.4).
//!
//! - [`detect`] resolves the binary (setting → `PATH` → common install
//!   locations), runs `claude --version` and `claude --help`, and reports the
//!   result as [`ClaudeCapabilities`] (or a [`DetectError`]).
//! - [`command`] is the single place where `claude` argument vectors are
//!   built: [`ClaudeCommand::review_run`] and [`ClaudeCommand::commit_message`].
//! - [`env`] builds a child's environment: the user's own for review runs,
//!   an allow list for probes and commit messages. Argus never forwards
//!   `ANTHROPIC_API_KEY` (or any `ANTHROPIC_*` variable); Claude Code
//!   authenticates with the user's existing sign-in.
//! - [`auth`] recognizes a failed sign-in in a run's output.

pub mod auth;
pub mod command;
pub mod detect;
pub mod env;

pub use auth::{AUTH_ERROR_HINT, is_auth_error};
pub use command::{ClaudeCommand, FALLBACK_PERMISSION_MODE, REVIEW_DISALLOWED_TOOLS, SessionMode};
pub use detect::{
    AUTO_MODE, ClaudeCapabilities, ClaudeStatus, ClaudeVersion, DetectError, EFFORT_FLAG,
    REQUIRED_FLAGS, detect,
};
