//! The `claude` CLI: finding it, checking what the installed version supports,
//! and building the exact command lines Argus runs (spec §6.2, §6.4).
//!
//! - [`detect`] resolves the binary (setting → `PATH` → common install
//!   locations), runs `claude --version` and `claude --help`, and reports the
//!   result as [`ClaudeCapabilities`] (or a [`DetectError`]).
//! - [`command`] is the single place where `claude` argument vectors are
//!   built: [`ClaudeCommand::review_run`] and [`ClaudeCommand::commit_message`].
//! - [`env`] is the allow list of environment variables a child inherits.
//!   Argus never reads, stores or forwards `ANTHROPIC_API_KEY`; Claude Code
//!   authenticates with the user's existing sign-in.
//! - [`auth`] recognizes a failed sign-in in a run's output.

pub mod auth;
pub mod command;
pub mod detect;
pub mod env;

pub use auth::{AUTH_ERROR_HINT, is_auth_error};
pub use command::{ClaudeCommand, LARGE_PROMPT_BYTES, SessionMode};
pub use detect::{
    ClaudeCapabilities, ClaudeStatus, ClaudeVersion, DetectError, EFFORT_FLAG, REQUIRED_FLAGS,
    detect,
};
