//! GPUI views, theme and app wiring for Argus.
//!
//! The binary crate stays thin: it boots logging and the tokio runtime, then
//! hands control to [`run_gpui_app`]. Service handles reach every view through
//! the [`AppContext`] global; colors and fonts through the [`Theme`] global.

pub mod accept;
mod assets;
pub mod branch_picker;
pub mod claude_settings;
pub mod claude_status;
pub mod comment_list;
pub mod comments;
pub mod components;
pub mod conflict;
pub mod diff_view;
pub mod file_tree;
pub mod followup;
mod fonts;
pub mod fuzzy;
pub mod icons;
pub mod jobs;
pub mod keymap;
pub mod markdown;
pub mod perf;
pub mod proposal;
pub mod push;
pub mod run_panel;
pub mod runtime;
pub mod session;
pub mod settings;
pub mod text_editor;
pub mod text_field;
pub mod theme;
mod workspace;

pub use assets::Assets;

use std::path::{Path, PathBuf};
use std::time::Instant;

use argus_git::{GitService, WorktreeManager};
use argus_highlight::HighlightService;
use argus_runs::RunManager;
use argus_store::StorageService;
use gpui::{
    App, Bounds, Global, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, prelude::*,
    px, size,
};

pub use comment_list::{CommentList, CommentListEvent};
pub use comments::{ReviewComments, ReviewCommentsEvent};
pub use diff_view::{
    CommentDrafts, DiffView, DiffViewEvent, DisplayOptions, DraftKey, LineSelection,
};
pub use file_tree::{FileTree, FileTreeEvent};
pub use jobs::BackgroundJobs;
pub use perf::PerfOptions;
pub use proposal::{ProposalEvent, ProposalSource, ProposalView};
pub use run_panel::{RunPanel, RunPanelEvent};
pub use session::{Comparison, DiffTarget, RepoSession, SessionEvent, Side};
pub use settings::{Settings, UiSettings};
pub use text_editor::{TextEditor, TextEditorEvent};
pub use theme::{
    ActiveTheme, Appearance, EYE_PUPIL, SyntaxTheme, Theme, ThemeColors, ThemeFamily, ThemeFonts,
    ThemeMode,
};
pub use workspace::Workspace;

/// Application-wide handles to the services, stored as a GPUI global.
///
/// Views read it with `cx.global::<AppContext>()`. The run manager carries the
/// live Claude settings, the detected `claude` capabilities and the live runs.
pub struct AppContext {
    /// Git reads (gix) and writes (git CLI).
    pub git: GitService,
    /// Run and integration worktrees under `~/.argus/worktrees/`.
    pub worktrees: WorktreeManager,
    /// SQLite storage in `~/.argus/argus.db`.
    pub store: StorageService,
    /// Claude run orchestration and the live run defaults.
    pub runs: RunManager,
    /// Background syntax highlighting (its own worker threads).
    pub highlighter: HighlightService,
    /// Handle to the process-wide tokio runtime. Service work runs there, never
    /// on the UI thread (spec §4.2).
    pub runtime: tokio::runtime::Handle,
}

impl Global for AppContext {}

impl AppContext {
    /// Builds the global from a live runtime handle, an opened store, the
    /// worktrees manager and the run manager.
    pub fn new(
        runtime: tokio::runtime::Handle,
        store: StorageService,
        git: GitService,
        worktrees: WorktreeManager,
        runs: RunManager,
    ) -> Self {
        Self {
            git,
            worktrees,
            store,
            runs,
            highlighter: HighlightService::new(),
            runtime,
        }
    }
}

/// A repository to open at launch instead of the most recent one, from the
/// command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRequest {
    /// Any path inside the repository.
    pub repo: PathBuf,
    /// Base branch; `None` keeps the remembered or default one.
    pub base: Option<String>,
    /// Head branch; `None` keeps the remembered or default one.
    pub head: Option<String>,
}

/// What the command line and the environment ask of the app.
#[derive(Clone, Debug)]
pub struct LaunchOptions {
    /// Open this repository at launch.
    pub open: Option<OpenRequest>,
    /// The `ARGUS_PERF` timing mode.
    pub perf: Option<PerfOptions>,
    /// Process start, for the perf report.
    pub launched: Instant,
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

/// Creates `<argus_home>/review-rules.md` with the default rules on first
/// launch. An existing file is never touched. Failures are logged, not fatal.
async fn ensure_default_review_rules(argus_home: &Path) {
    let path = argus_runs::settings::default_review_rules_path(argus_home);
    match argus_runs::review_rules::ensure_review_rules(&path).await {
        Ok(true) => tracing::info!(path = %path.display(), "created default review rules"),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot create review rules");
        }
    }
}

/// Opens the Argus window and runs the GPUI event loop until the app quits.
///
/// Blocks the calling thread, which must be the main thread (macOS requires the
/// platform run loop there). The settings are read, and the default review
/// rules created, on the tokio runtime before the event loop starts; then the
/// thread is marked as the UI thread, so debug builds panic on any storage call
/// made from it.
///
/// `launch` carries the command line: a repository to open instead of the
/// most recent one, and the `ARGUS_PERF` timing mode (see [`perf`]).
pub fn run_gpui_app(
    runtime: tokio::runtime::Handle,
    store: StorageService,
    worktrees: WorktreeManager,
    argus_home: PathBuf,
    launch: LaunchOptions,
) {
    let git = GitService::new();
    runtime.spawn(reconcile_worktrees(
        git.clone(),
        store.clone(),
        worktrees.clone(),
    ));

    let settings = runtime.block_on({
        let store = store.settings();
        let argus_home = argus_home.clone();
        async move {
            ensure_default_review_rules(&argus_home).await;
            Settings::load(&store).await
        }
    });
    let ui_settings = settings.ui;
    let runs = RunManager::new(
        argus_home,
        settings.claude,
        store.clone(),
        git.clone(),
        worktrees.clone(),
    );
    // `claude --version` / `--help` run on the runtime; the UI only receives
    // the resulting status.
    runs.start_detection(&runtime);

    argus_store::mark_ui_thread();
    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            cx.set_global(AppContext::new(
                runtime.clone(),
                store.clone(),
                git.clone(),
                worktrees.clone(),
                runs.clone(),
            ));
            cx.set_global(BackgroundJobs::default());
            if let Some(options) = launch.perf {
                perf::install(options, launch.launched, cx);
            }
            // Before any font is resolved: the bundled IBM Plex families head
            // both preference lists.
            fonts::register_bundled_fonts(cx);
            // A provisional theme so views can render before the workspace
            // resolves fonts and the window appearance.
            cx.set_global(Theme::new(
                ui_settings.theme_family,
                Appearance::Dark,
                ThemeFonts::default(),
            ));

            cx.set_reduce_motion(ui_settings.reduce_motion);

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
                    let open = launch.open.clone();
                    cx.new(|cx| Workspace::new(settings, open, window, cx))
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
