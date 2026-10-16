//! GPUI views, theme and app wiring for Argus.
//!
//! The binary crate stays thin: it boots logging and the tokio runtime, then
//! hands control to [`run_gpui_app`]. Service handles reach every view through
//! the [`AppContext`] global.

use argus_git::GitService;
use argus_runs::RunManager;
use argus_store::StorageService;
use gpui::{
    App, Bounds, Context, Global, IntoElement, Render, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, size,
};

/// Application-wide handles to the services, stored as a GPUI global.
///
/// Views read it with `cx.global::<AppContext>()`. The services are filled in
/// by later issues (#3 storage, #4/#17 git, #20 runs); for now they are
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
    /// Builds the global from a live runtime handle.
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            git: GitService::new(),
            store: StorageService::new(),
            runs: RunManager::new(),
            runtime,
        }
    }
}

/// Opens the Argus window and runs the GPUI event loop until the app quits.
///
/// Blocks the calling thread, which must be the main thread (macOS requires the
/// platform run loop there).
pub fn run_gpui_app(runtime: tokio::runtime::Handle) {
    gpui_platform::application().run(move |cx: &mut App| {
        cx.set_global(AppContext::new(runtime.clone()));

        let bounds = Bounds::centered(None, size(px(1200.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Argus".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| RootView),
        )
        .expect("failed to open the Argus window");

        cx.activate(true);
    });
}

/// The empty window root.
///
/// Issue #5 replaces this with the Zed-style app shell (title bar, docks, diff
/// area, status bar).
struct RootView;

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}
