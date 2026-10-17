//! Claude runs for Argus: prompt builder, run manager and the `stream-json`
//! parser.
//!
//! - [`prompt`] renders the versioned prompts of comment, follow-up and
//!   resolve runs.
//! - [`claude`] detects the installed CLI and builds its exact command lines.
//! - [`stream`] parses the `stream-json` lines a run prints.
//! - [`run`] (through [`RunManager::start`] and [`RunManager::stop`]) spawns
//!   one `claude -p` per run in its own worktree, streams and stores its
//!   events, and records the outcome (spec §6.3).
//! - [`proposal`] shows what a finished run changed (`start_sha → worktree`)
//!   and rejects it ([`RunManager::open_proposal`], [`RunManager::reject`]).
//! - The run defaults of the settings screen live in [`settings`]; the
//!   review rules file in [`review_rules`].
//!
//! Runs interrupted by a crash are marked `Failed (interrupted)` at startup
//! by [`argus_git::WorktreeManager::mark_interrupted_runs`], before any run
//! of the new session starts.

pub mod claude;
pub mod prompt;
pub mod proposal;
pub mod review_rules;
pub mod run;
pub mod settings;
pub mod stream;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use argus_git::{GitService, WorktreeManager};
use argus_store::{CommentId, RunId, StorageService};
use tokio::sync::{Notify, broadcast, watch};

pub use claude::{ClaudeCapabilities, ClaudeCommand, ClaudeStatus, SessionMode};
pub use proposal::{Proposal, ProposalError, WorktreeWatch};
pub use run::{RunError, RunFinished, RunStreamEvent, RunUpdate, STOP_GRACE};
pub use settings::{ClaudeSettings, SettingError};
pub use stream::StreamEvent;

/// Handle to the run manager.
///
/// It spawns and tracks `claude -p` child processes, each in its own
/// worktree, with no queue and no limit ([`RunManager::start`],
/// [`RunManager::stop`]). Lifecycle changes are broadcast to
/// [`RunManager::subscribe_runs`], stream events to
/// [`RunManager::subscribe_events`].
///
/// It holds the live [`ClaudeSettings`]: the settings screen publishes every
/// saved change here, and each run reads a fresh snapshot when it starts, so
/// a new Claude path, default model or effort applies to the next run
/// without a restart.
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
    services: Arc<run::Services>,
}

impl RunManager {
    /// Creates the service handle with the settings loaded at startup and
    /// the services runs use.
    pub fn new(
        argus_home: PathBuf,
        settings: ClaudeSettings,
        store: StorageService,
        git: GitService,
        worktrees: WorktreeManager,
    ) -> Self {
        let (sender, _) = watch::channel(settings);
        let (claude, _) = watch::channel(ClaudeStatus::Detecting);
        Self {
            argus_home: Arc::new(argus_home),
            settings: Arc::new(sender),
            claude: Arc::new(claude),
            recheck: Arc::new(Notify::new()),
            services: Arc::new(run::Services::new(store, git, worktrees)),
        }
    }

    /// Starts a run for a comment with `model` and `effort` (an empty
    /// effort, or a CLI without `--effort`, uses Claude's default).
    ///
    /// Records the run (`kind = comment`, a new session), creates its
    /// worktree at the current head tip, builds the prompt and the command,
    /// moves the comment to `Running` and spawns `claude`. Returns once the
    /// process runs; an independent task streams it to completion. Must run
    /// on the tokio runtime, never on the UI thread.
    pub async fn start(
        &self,
        comment_id: CommentId,
        model: impl Into<String>,
        effort: impl Into<String>,
    ) -> Result<RunId, RunError> {
        let status = self.claude_status();
        if let Some(blocker) = status.run_blocker() {
            return Err(RunError::ClaudeUnavailable(blocker));
        }
        let Some(capabilities) = status.capabilities().cloned() else {
            return Err(RunError::ClaudeUnavailable(
                "Claude Code is unavailable".to_owned(),
            ));
        };
        run::start(
            self.services.clone(),
            run::StartRequest {
                comment_id,
                model: model.into(),
                effort: effort.into(),
                capabilities,
                rules_path: self.review_rules_path(),
            },
        )
        .await
    }

    /// Stops a run: `SIGTERM` to its process group, `SIGKILL` after
    /// [`STOP_GRACE`]. It ends `Stopped`, with its worktree kept. Returns
    /// `false` when the run is not running. Never blocks.
    pub fn stop(&self, run_id: RunId) -> bool {
        self.services.stop(run_id)
    }

    /// The latest run of a comment with a change to review (succeeded, or
    /// stopped with its partial change kept).
    pub async fn proposal_run(&self, comment_id: CommentId) -> Result<RunId, ProposalError> {
        proposal::latest_run(&self.services, comment_id).await
    }

    /// Opens the proposal of a finished run: snapshots its worktree and
    /// prepares the diff `start_sha → worktree`, with the files outside the
    /// comment's file listed first. Must run on the tokio runtime.
    pub async fn open_proposal(&self, run_id: RunId) -> Result<Proposal, ProposalError> {
        proposal::open(&self.services, run_id).await
    }

    /// Snapshots a proposal's worktree again (after a manual edit). `None`
    /// when neither the worktree nor the head branch changed.
    pub async fn refresh_proposal(
        &self,
        proposal: &Proposal,
    ) -> Result<Option<Proposal>, ProposalError> {
        proposal::refresh(&self.services, proposal).await
    }

    /// Watches a proposal's worktree for edits made outside Argus. Must be
    /// called on the tokio runtime.
    pub fn watch_proposal(&self, proposal: &Proposal) -> WorktreeWatch {
        proposal::watch(proposal.worktree.path())
    }

    /// Rejects a run's proposal: deletes its worktree (`git worktree remove
    /// --force` + `prune`), records the run as `Rejected` and moves its
    /// comment back to `Open`. Publishes [`RunUpdate::Rejected`].
    pub async fn reject(&self, run_id: RunId) -> Result<(), ProposalError> {
        proposal::reject(&self.services, run_id).await
    }

    /// Whether the run's process is live in this session.
    pub fn is_running(&self, run_id: RunId) -> bool {
        self.services.is_active(run_id)
    }

    /// The runs whose process is live in this session.
    pub fn active_runs(&self) -> Vec<RunId> {
        self.services.active_runs()
    }

    /// Observes run starts and outcomes.
    pub fn subscribe_runs(&self) -> broadcast::Receiver<RunUpdate> {
        self.services.subscribe_status()
    }

    /// Observes the stream events of every live run. A lagging receiver can
    /// replay a run from [`argus_store::RunStore::events`].
    pub fn subscribe_events(&self) -> broadcast::Receiver<RunStreamEvent> {
        self.services.subscribe_events()
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
