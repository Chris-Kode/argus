//! Claude runs for Argus: prompt builder, run manager and the `stream-json`
//! parser.
//!
//! Filled in by issues #19 (prompt builder), #20 (run manager: spawn
//! `claude -p`, parse `stream-json`, parallel runs, stop) and #21–#23 (run
//! menu, proposal review, follow-up replies).
//!
//! [`prompt`] renders the versioned prompts of comment, follow-up and
//! resolve runs.

pub mod prompt;

/// Handle to the run manager.
///
/// Placeholder until issue #20 lands: it will spawn and track `claude -p`
/// child processes, each in its own worktree, with unlimited parallelism.
#[derive(Clone, Debug, Default)]
pub struct RunManager;

impl RunManager {
    /// Creates the service handle.
    pub fn new() -> Self {
        Self
    }
}
