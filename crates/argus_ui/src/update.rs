//! The in-app updater: looks for a newer release in the background and, on
//! request, installs it over the running `Argus.app` and restarts.
//!
//! The checks and the install run on the tokio runtime (see
//! [`argus_update`]); this entity only holds the state the title bar shows.

use std::path::PathBuf;
use std::time::Duration;

use argus_update::Release;
use gpui::{Context, EventEmitter, PromptLevel, SharedString, Task, Window};

use crate::AppContext;
use crate::runtime::spawn_job;

/// The first check waits for the launch to settle.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
/// Later checks, while the app stays open.
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// Where the updater stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// No newer release known.
    Idle,
    /// A newer release can be installed.
    Available(Release),
    /// The release is downloading or being put in place; the app restarts
    /// when it is done.
    Installing(Release),
}

/// Something for the status bar.
#[derive(Clone, Debug)]
pub struct UpdaterEvent {
    /// The text to show.
    pub message: SharedString,
    /// Shown as an error rather than a note.
    pub is_error: bool,
}

/// Background release checks and the install of a newer one.
pub struct Updater {
    /// The running bundle; `None` when Argus does not run as `Argus.app`
    /// (e.g. `cargo run`), which never updates itself.
    app: Option<PathBuf>,
    /// Downloads go to `<argus_home>/updates`.
    updates_dir: PathBuf,
    status: UpdateStatus,
    /// Why the bundle cannot be replaced in place, once known.
    blocker: Option<SharedString>,
    check_task: Option<Task<()>>,
    install_task: Option<Task<()>>,
    _schedule: Task<()>,
    _blocker_task: Task<()>,
}

impl EventEmitter<UpdaterEvent> for Updater {}

impl Updater {
    /// Starts the periodic checks when Argus runs as an app bundle.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let app = cx
            .app_path()
            .ok()
            .filter(|path| path.extension().is_some_and(|ext| ext == "app"));
        let updates_dir = cx.global::<AppContext>().runs.argus_home().join("updates");

        let schedule = if app.is_some() {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(FIRST_CHECK_DELAY).await;
                loop {
                    if this.update(cx, |this, cx| this.check(false, cx)).is_err() {
                        return;
                    }
                    cx.background_executor().timer(CHECK_INTERVAL).await;
                }
            })
        } else {
            Task::ready(())
        };
        let blocker_task = match app.clone() {
            Some(app) => {
                let job = spawn_job(cx, async move { argus_update::install_blocker(&app).await });
                cx.spawn(async move |this, cx| {
                    if let Ok(blocker) = job.await {
                        this.update(cx, |this, cx| {
                            this.blocker = blocker.map(SharedString::from);
                            cx.notify();
                        })
                        .ok();
                    }
                })
            }
            None => Task::ready(()),
        };

        Self {
            app,
            updates_dir,
            status: UpdateStatus::Idle,
            blocker: None,
            check_task: None,
            install_task: None,
            _schedule: schedule,
            _blocker_task: blocker_task,
        }
    }

    /// The current state.
    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    /// Why an available update cannot be installed here, if it cannot.
    pub fn blocker(&self) -> Option<&SharedString> {
        self.blocker.as_ref()
    }

    /// Asks GitHub for a newer release. `manual` checks (the menu item)
    /// report their outcome, including "up to date" and failures.
    pub fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.app.is_none() {
            if manual {
                self.notify(
                    "Argus updates itself only when it runs as Argus.app.",
                    false,
                    cx,
                );
            }
            return;
        }
        if matches!(self.status, UpdateStatus::Installing(_)) || self.check_task.is_some() {
            return;
        }
        let job = spawn_job(cx, argus_update::check());
        self.check_task = Some(cx.spawn(async move |this, cx| {
            let Ok(result) = job.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.check_task = None;
                if matches!(this.status, UpdateStatus::Installing(_)) {
                    return;
                }
                match result {
                    Ok(Some(release)) => {
                        tracing::info!(version = %release.version, "update available");
                        this.status = UpdateStatus::Available(release);
                    }
                    Ok(None) => {
                        this.status = UpdateStatus::Idle;
                        if manual {
                            let current = argus_update::current_version();
                            this.notify(format!("Argus {current} is up to date."), false, cx);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "update check failed");
                        if manual {
                            this.notify(format!("Cannot check for updates: {error}"), true, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Installs the available release and restarts. Asks first when Claude
    /// runs are in flight, since the restart stops them.
    pub fn install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let UpdateStatus::Available(release) = &self.status else {
            return;
        };
        if self.blocker.is_some() {
            return;
        }
        let release = release.clone();
        let active = cx.global::<AppContext>().runs.active_runs().len();
        if active == 0 {
            self.start_install(release, cx);
            return;
        }
        let detail = match active {
            1 => "Restarting stops the Claude run in progress.".to_owned(),
            n => format!("Restarting stops the {n} Claude runs in progress."),
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Update to Argus {} and restart?", release.version),
            Some(&detail),
            &["Update and Restart", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |this, cx| this.start_install(release, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn start_install(&mut self, release: Release, cx: &mut Context<Self>) {
        let Some(app) = self.app.clone() else {
            return;
        };
        if matches!(self.status, UpdateStatus::Installing(_)) {
            return;
        }
        self.status = UpdateStatus::Installing(release.clone());
        cx.notify();
        let updates_dir = self.updates_dir.clone();
        let job = spawn_job(cx, {
            let release = release.clone();
            async move { argus_update::install(&release, &app, &updates_dir).await }
        });
        self.install_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.install_task = None;
                match result {
                    Ok(Ok(())) => cx.restart(),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "update failed");
                        this.status = UpdateStatus::Available(release);
                        this.notify(format!("Update failed: {error}"), true, cx);
                        cx.notify();
                    }
                    Err(_) => {}
                }
            })
            .ok();
        }));
    }

    fn notify(&self, message: impl Into<SharedString>, is_error: bool, cx: &mut Context<Self>) {
        cx.emit(UpdaterEvent {
            message: message.into(),
            is_error,
        });
    }
}
