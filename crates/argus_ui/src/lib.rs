//! GPUI views, theme and app wiring for Argus.
//!
//! The binary crate stays thin: it boots logging and the tokio runtime, then
//! hands control to [`run_gpui_app`]. Service handles reach every view through
//! the [`AppContext`] global; colors and fonts through the [`Theme`] global.

pub mod branch_picker;
pub mod comments;
pub mod components;
pub mod diff_view;
pub mod file_tree;
mod fonts;
pub mod fuzzy;
pub mod jobs;
pub mod keymap;
pub mod runtime;
pub mod session;
pub mod settings;
pub mod theme;
mod workspace;

use std::path::Path;

use argus_git::{GitService, WorktreeManager};
use argus_highlight::HighlightService;
use argus_runs::RunManager;
use argus_store::StorageService;
use gpui::{
    App, Bounds, Global, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, prelude::*,
    px, size,
};

pub use comments::{ReviewComments, ReviewCommentsEvent};
pub use diff_view::{DiffView, DiffViewEvent, DisplayOptions, LineSelection};
pub use file_tree::{FileTree, FileTreeEvent};
pub use jobs::BackgroundJobs;
pub use session::{Comparison, DiffTarget, RepoSession, SessionEvent, Side};
pub use settings::UiSettings;
pub use theme::{ActiveTheme, Appearance, SyntaxTheme, Theme, ThemeColors, ThemeFonts, ThemeMode};
pub use workspace::Workspace;

/// Application-wide handles to the services, stored as a GPUI global.
///
/// Views read it with `cx.global::<AppContext>()`. The run manager is still a
/// placeholder handle until issue #20 lands.
pub struct AppContext {
    /// Git reads (gix) and writes (git CLI).
    pub git: GitService,
    /// Run and integration worktrees under `~/.argus/worktrees/`.
    pub worktrees: WorktreeManager,
    /// SQLite storage in `~/.argus/argus.db`.
    pub store: StorageService,
    /// Claude run orchestration.
    pub runs: RunManager,
    /// Background syntax highlighting (its own worker threads).
    pub highlighter: HighlightService,
    /// Handle to the process-wide tokio runtime. Service work runs there, never
    /// on the UI thread (spec §4.2).
    pub runtime: tokio::runtime::Handle,
}

impl Global for AppContext {}

impl AppContext {
    /// Builds the global from a live runtime handle, an opened store and the
    /// worktrees manager.
    pub fn new(
        runtime: tokio::runtime::Handle,
        store: StorageService,
        git: GitService,
        worktrees: WorktreeManager,
    ) -> Self {
        Self {
            git,
            worktrees,
            store,
            runs: RunManager::new(),
            highlighter: HighlightService::new(),
            runtime,
        }
    }
}

/// Opens the worktrees root under `argus_home` and marks the runs a previous
/// session left queued or running as `Failed (interrupted)`.
///
/// Run it on the tokio runtime before the UI starts, so no run of the new
/// session can be mistaken for an interrupted one.
pub async fn open_worktrees(
    argus_home: &Path,
    store: &StorageService,
) -> argus_git::Result<WorktreeManager> {
    let worktrees = WorktreeManager::open(argus_home).await?;
    let interrupted = WorktreeManager::mark_interrupted_runs(store).await?;
    if !interrupted.is_empty() {
        tracing::info!(
            count = interrupted.len(),
            "marked interrupted runs as failed"
        );
    }
    reopen_interrupted_comments(store, &interrupted).await;
    Ok(worktrees)
}

/// Moves the comments of interrupted runs from `Running` back to `Open`, as
/// for any failed run (spec §6.3). Failures are logged, not fatal.
async fn reopen_interrupted_comments(store: &StorageService, interrupted: &[argus_store::RunId]) {
    let comments = store.comments();
    for &run_id in interrupted {
        let comment_id = match store.runs().get(run_id).await {
            Ok(Some(run)) => run.comment_id,
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(run = %run_id, %error, "cannot read interrupted run");
                None
            }
        };
        let Some(comment_id) = comment_id else {
            continue;
        };
        match comments.get(comment_id).await {
            Ok(Some(comment)) if comment.status == argus_store::CommentStatus::Running => {}
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!(comment = %comment_id, %error, "cannot read comment");
                continue;
            }
        }
        let event = argus_diff::review::CommentEvent::RunFailed;
        if let Err(error) = argus_diff::review::apply_event(&comments, comment_id, event).await {
            tracing::warn!(comment = %comment_id, %error, "cannot reopen comment");
        }
    }
}

/// Startup reconciliation of every known repository's worktrees (spec §5.2),
/// in the background. Repositories that no longer open are skipped.
async fn reconcile_worktrees(git: GitService, store: StorageService, worktrees: WorktreeManager) {
    let repos = match store.repos().list().await {
        Ok(repos) => repos,
        Err(error) => {
            tracing::warn!(%error, "cannot list repositories for worktree reconciliation");
            return;
        }
    };
    for record in repos {
        let repo = match git.open(record.path.clone()).await {
            Ok(repo) => repo,
            Err(error) => {
                tracing::debug!(path = %record.path.display(), %error, "skipping reconciliation");
                continue;
            }
        };
        if let Err(error) = worktrees.reconcile(&repo, &store).await {
            tracing::warn!(path = %record.path.display(), %error, "worktree reconciliation failed");
        }
    }
}

/// Opens the Argus window and runs the GPUI event loop until the app quits.
///
/// Blocks the calling thread, which must be the main thread (macOS requires the
/// platform run loop there). The UI settings are read on the tokio runtime
/// before the event loop starts; then the thread is marked as the UI thread, so
/// debug builds panic on any storage call made from it.
pub fn run_gpui_app(
    runtime: tokio::runtime::Handle,
    store: StorageService,
    worktrees: WorktreeManager,
) {
    let git = GitService::new();
    runtime.spawn(reconcile_worktrees(
        git.clone(),
        store.clone(),
        worktrees.clone(),
    ));

    let ui_settings = runtime.block_on({
        let store = store.settings();
        async move { UiSettings::load(&store).await }
    });

    argus_store::mark_ui_thread();
    gpui_platform::application().run(move |cx: &mut App| {
        cx.set_global(AppContext::new(
            runtime.clone(),
            store.clone(),
            git.clone(),
            worktrees.clone(),
        ));
        cx.set_global(BackgroundJobs::default());
        // A provisional theme so views can render before the workspace
        // resolves fonts and the window appearance.
        cx.set_global(Theme::new(Appearance::Dark, ThemeFonts::default()));

        keymap::bind_keys(cx);
        cx.on_action(|_: &keymap::Quit, cx| cx.quit());
        cx.set_menus([Menu::new("Argus").items([
            MenuItem::action("Open Repository…", keymap::OpenRepository),
            MenuItem::action("Settings…", keymap::OpenSettings),
            MenuItem::action("Keyboard Shortcuts", keymap::ToggleKeyboardShortcuts),
            MenuItem::separator(),
            MenuItem::action("Quit Argus", keymap::Quit),
        ])]);
        // Linux and Windows keep running without windows otherwise.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Argus".into()),
                    // macOS draws the traffic lights over Argus' own title bar.
                    appears_transparent: cfg!(target_os = "macos"),
                    traffic_light_position: Some(gpui::point(px(9.0), px(10.0))),
                }),
                window_min_size: Some(size(px(720.0), px(480.0))),
                ..Default::default()
            },
            |window, cx| {
                let settings = ui_settings.clone();
                cx.new(|cx| Workspace::new(settings, window, cx))
            },
        );
        if let Err(error) = window {
            tracing::error!(%error, "failed to open the Argus window");
            cx.quit();
            return;
        }

        cx.activate(true);
    });
}
