//! Claude runs for Argus: prompt builder, run manager and the `stream-json`
//! parser.
//!
//! Filled in by issues #19 (prompt builder), #20 (run manager: spawn
//! `claude -p`, parse `stream-json`, parallel runs, stop) and #21–#23 (run
//! menu, proposal review, follow-up replies). The run defaults of the
//! settings screen live in [`settings`]; the review rules file in
//! [`review_rules`].

pub mod review_rules;
pub mod settings;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::watch;

pub use settings::{ClaudeSettings, SettingError};

/// Handle to the run manager.
///
/// It will spawn and track `claude -p` child processes, each in its own
/// worktree, with unlimited parallelism (issue #20). Today it holds the live
/// [`ClaudeSettings`]: the settings screen publishes every saved change here,
/// and each run reads a fresh snapshot when it starts, so a new Claude path,
/// default model or effort applies to the next run without a restart.
#[derive(Clone, Debug)]
pub struct RunManager {
    argus_home: Arc<PathBuf>,
    settings: Arc<watch::Sender<ClaudeSettings>>,
}

impl RunManager {
    /// Creates the service handle with the settings loaded at startup.
    pub fn new(argus_home: PathBuf, settings: ClaudeSettings) -> Self {
        let (sender, _) = watch::channel(settings);
        Self {
            argus_home: Arc::new(argus_home),
            settings: Arc::new(sender),
        }
    }

    /// The Argus home directory (`~/.argus` or `ARGUS_HOME`).
    pub fn argus_home(&self) -> &Path {
        &self.argus_home
    }

    /// A snapshot of the current settings; take one per run.
    pub fn settings(&self) -> ClaudeSettings {
        self.settings.borrow().clone()
    }

    /// Replaces the settings; subscribers are notified only on a change.
    pub fn set_settings(&self, settings: ClaudeSettings) {
        self.settings.send_if_modified(|current| {
            if *current == settings {
                false
            } else {
                *current = settings;
                true
            }
        });
    }

    /// Observes the settings: the receiver sees every later change.
    pub fn subscribe(&self) -> watch::Receiver<ClaudeSettings> {
        self.settings.subscribe()
    }

    /// The review rules file the next run appends to its system prompt.
    pub fn review_rules_path(&self) -> PathBuf {
        self.settings.borrow().review_rules_path(&self.argus_home)
    }
}
