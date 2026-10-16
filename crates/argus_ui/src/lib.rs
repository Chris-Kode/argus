//! GPUI views, theme and app wiring for Argus.
//!
//! The binary crate stays thin: it boots logging and the tokio runtime, then
//! hands control to [`run_gpui_app`]. Service handles reach every view through
//! the [`AppContext`] global; colors and fonts through the [`Theme`] global.

pub mod components;
mod fonts;
pub mod jobs;
pub mod keymap;
pub mod settings;
pub mod theme;
mod workspace;

use argus_git::GitService;
use argus_runs::RunManager;
use argus_store::StorageService;
use gpui::{
    App, Bounds, Global, Menu, MenuItem, TitlebarOptions, WindowBounds, WindowOptions, prelude::*,
    px, size,
};

pub use jobs::BackgroundJobs;
pub use settings::UiSettings;
pub use theme::{ActiveTheme, Appearance, Theme, ThemeColors, ThemeFonts, ThemeMode};
pub use workspace::Workspace;

/// Application-wide handles to the services, stored as a GPUI global.
///
/// Views read it with `cx.global::<AppContext>()`. The git and run services are
/// filled in by later issues (#4/#17 git, #20 runs); for now they are
/// placeholder handles so the wiring exists from the start.
pub struct AppContext {
    /// Git reads (gix) and writes (git CLI).
    pub git: GitService,
    /// SQLite storage in `~/.argus/argus.db`.
    pub store: StorageService,
    /// Claude run orchestration.
    pub runs: RunManager,
    /// Handle to the process-wide tokio runtime. Service work runs there, never
    /// on the UI thread (spec §4.2).
    pub runtime: tokio::runtime::Handle,
}

impl Global for AppContext {}

impl AppContext {
    /// Builds the global from a live runtime handle and an opened store.
    pub fn new(runtime: tokio::runtime::Handle, store: StorageService) -> Self {
        Self {
            git: GitService::new(),
            store,
            runs: RunManager::new(),
            runtime,
        }
    }
}

/// Opens the Argus window and runs the GPUI event loop until the app quits.
///
/// Blocks the calling thread, which must be the main thread (macOS requires the
/// platform run loop there). The UI settings are read on the tokio runtime
/// before the event loop starts; then the thread is marked as the UI thread, so
/// debug builds panic on any storage call made from it.
pub fn run_gpui_app(runtime: tokio::runtime::Handle, store: StorageService) {
    let ui_settings = runtime.block_on({
        let store = store.settings();
        async move { UiSettings::load(&store).await }
    });

    argus_store::mark_ui_thread();
    gpui_platform::application().run(move |cx: &mut App| {
        cx.set_global(AppContext::new(runtime.clone(), store.clone()));
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
