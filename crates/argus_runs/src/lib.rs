//! Claude runs for Argus: prompt builder, run manager and the `stream-json`
//! parser.
//!
//! Filled in by issues #19 (prompt builder), #20 (run manager: spawn
//! `claude -p`, parse `stream-json`, parallel runs, stop) and #21–#23 (run
//! menu, proposal review, follow-up replies).
//!
//! [`prompt`] renders the versioned prompts of comment, follow-up and
//! resolve runs. The run defaults of the settings screen live in
//! [`settings`]; the review rules file in [`review_rules`]. [`claude`]
//! detects the installed CLI and builds its exact command lines.

pub mod claude;
pub mod prompt;
pub mod review_rules;
pub mod settings;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Notify, watch};

pub use claude::{ClaudeCapabilities, ClaudeCommand, ClaudeStatus, SessionMode};
pub use settings::{ClaudeSettings, SettingError};

/// Handle to the run manager.
///
/// It will spawn and track `claude -p` child processes, each in its own
/// worktree, with unlimited parallelism (issue #20). Today it holds the live
/// [`ClaudeSettings`]: the settings screen publishes every saved change here,
/// and each run reads a fresh snapshot when it starts, so a new Claude path,
/// default model or effort applies to the next run without a restart.
///
/// It also publishes the [`ClaudeStatus`] of the installed `claude`, detected
/// at startup and again whenever the `claude_path` setting changes (see
/// [`RunManager::start_detection`]).
#[derive(Clone, Debug)]
pub struct RunManager {
    argus_home: Arc<PathBuf>,
    settings: Arc<watch::Sender<ClaudeSettings>>,
    claude: Arc<watch::Sender<ClaudeStatus>>,
    recheck: Arc<Notify>,
}

impl RunManager {
    /// Creates the service handle with the settings loaded at startup.
    pub fn new(argus_home: PathBuf, settings: ClaudeSettings) -> Self {
        let (sender, _) = watch::channel(settings);
        let (claude, _) = watch::channel(ClaudeStatus::Detecting);
        Self {
            argus_home: Arc::new(argus_home),
            settings: Arc::new(sender),
            claude: Arc::new(claude),
            recheck: Arc::new(Notify::new()),
        }
    }

    /// Starts detecting `claude` on `runtime`: now, again whenever the
    /// `claude_path` setting changes (an in-flight detection is cancelled),
    /// and on [`RunManager::recheck_claude`]. Call it once, at startup.
    pub fn start_detection(&self, runtime: &tokio::runtime::Handle) {
        runtime.spawn(detection_loop(self.clone()));
    }

    /// The latest detection result.
    pub fn claude_status(&self) -> ClaudeStatus {
        self.claude.borrow().clone()
    }

    /// Observes the detection: the receiver sees every later status.
    pub fn subscribe_claude(&self) -> watch::Receiver<ClaudeStatus> {
        self.claude.subscribe()
    }

    /// Detects `claude` again with the current settings, e.g. after the user
    /// installed or updated it.
    pub fn recheck_claude(&self) {
        self.recheck.notify_one();
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

/// Why the detection loop starts over.
enum Restart {
    /// The `claude_path` setting changed to this value.
    Path(Option<PathBuf>),
    /// The user asked for a new check.
    Recheck,
    /// The run manager is gone.
    Stop,
}

/// Detects `claude`, then waits for a reason to detect again.
async fn detection_loop(manager: RunManager) {
    let mut settings = manager.subscribe();
    let mut path = settings.borrow_and_update().claude_path.clone();
    loop {
        manager.claude.send_replace(ClaudeStatus::Detecting);
        let restart = tokio::select! {
            result = claude::detect(path.clone()) => {
                let status = match result {
                    Ok(capabilities) => ClaudeStatus::Detected(capabilities),
                    Err(error) => {
                        tracing::warn!(%error, "Claude Code is unavailable");
                        ClaudeStatus::Unavailable(error)
                    }
                };
                manager.claude.send_replace(status);
                next_restart(&manager, &mut settings, &path).await
            }
            restart = next_restart(&manager, &mut settings, &path) => restart,
        };
        match restart {
            Restart::Path(new_path) => path = new_path,
            Restart::Recheck => {}
            Restart::Stop => return,
        }
    }
}

/// Waits for a `claude_path` different from `current`, or a recheck request.
async fn next_restart(
    manager: &RunManager,
    settings: &mut watch::Receiver<ClaudeSettings>,
    current: &Option<PathBuf>,
) -> Restart {
    loop {
        tokio::select! {
            changed = settings.changed() => {
                if changed.is_err() {
                    return Restart::Stop;
                }
                let path = settings.borrow_and_update().claude_path.clone();
                if path != *current {
                    return Restart::Path(path);
                }
            }
            () = manager.recheck.notified() => return Restart::Recheck,
        }
    }
}
