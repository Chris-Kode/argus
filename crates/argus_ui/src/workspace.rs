//! The Zed-style app shell: title bar, file-tree dock, center tabs, run-panel
//! dock, status bar, and the settings and keyboard-shortcuts sheets.
//!
//! The open repository and its base/head selection live in a [`RepoSession`];
//! the title bar hosts its branch pickers, swap and Fetch buttons. The viewer
//! (#7/#8), comments and runs (#21) plug their views into the slots rendered
//! here. Settings, the repository list and git are read and written on the
//! tokio runtime; this view only receives the results.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use argus_store::{Repo, StorageService};
use gpui::{
    Action, AnyElement, Context, DragMoveEvent, EmptyView, Entity, FocusHandle, Focusable,
    MouseButton, PathPromptOptions, Pixels, Point, SharedString, Subscription, Task, Window,
    WindowControlArea, deferred, div, prelude::*, px,
};

use crate::AppContext;
use crate::branch_picker::{BranchPicker, PickerEvent};
use crate::components::{
    button, disabled_button, kbd, panel_header, placeholder, primary_button, toggle_button,
};
use crate::file_tree::{FileTree, FileTreeEvent};
use crate::fonts::InstalledFonts;
use crate::jobs::BackgroundJobs;
use crate::keymap::{
    Accept, Comment, CycleTheme, DecreaseFontSize, Dismiss, FocusFileFilter, IncreaseFontSize,
    NextFile, NextHunk, OpenRepository, OpenSettings, PreviousFile, PreviousHunk, ResetFontSize,
    Run, SHORTCUT_GROUPS, SHORTCUTS, ToggleFileListMode, ToggleFileTree, ToggleKeyboardShortcuts,
    ToggleRunPanel, ToggleRunPanelPosition, WORKSPACE_CONTEXT,
};
use crate::runtime::spawn_job;
use crate::session::{
    BranchList, Comparison, DiffTarget, RepoSession, SessionEvent, Side, open_repository,
};
use crate::settings::{
    DEFAULT_CODE_FONT_SIZE, DEFAULT_FILE_TREE_WIDTH, DEFAULT_RUN_PANEL_HEIGHT,
    DEFAULT_RUN_PANEL_WIDTH, DEFAULT_UI_FONT_SIZE, MIN_DOCK_SIZE, RunPanelPosition, UiSettings,
    clamp_font_size,
};
use crate::theme::{ActiveTheme, Theme, ThemeFonts, ThemeMode};

/// Height of the title bar.
const TITLE_BAR_HEIGHT: Pixels = px(36.0);
/// Height of the status bar.
const STATUS_BAR_HEIGHT: Pixels = px(26.0);
/// Space kept for the center area when docks grow.
const MIN_CENTER_SIZE: f32 = 240.0;
/// Width of the invisible grab area of a dock resize handle.
const RESIZE_HANDLE_SIZE: Pixels = px(6.0);
/// Settings are written this long after the last change, so a dock drag
/// produces one write instead of one per mouse move.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
/// How many recent repositories the empty state lists.
const RECENT_REPOS_SHOWN: usize = 10;
/// A picker closed this recently by a click outside it is not reopened by
/// the same click landing on its own title-bar button.
const PICKER_REOPEN_GUARD: Duration = Duration::from_millis(250);

/// Room for the macOS traffic lights over the transparent title bar.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHTS_WIDTH: Pixels = px(76.0);
#[cfg(not(target_os = "macos"))]
const TRAFFIC_LIGHTS_WIDTH: Pixels = px(8.0);

/// Which dock edge is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DockResize {
    FileTree,
    RunPanel,
}

/// A sheet shown over the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sheet {
    KeyboardShortcuts,
    Settings,
}

/// The recent-repositories list in the empty state.
enum RecentRepos {
    Loading,
    Loaded(Vec<Repo>),
    Failed(SharedString),
}

/// A one-line message in the status bar.
struct Notice {
    message: SharedString,
    is_error: bool,
}

/// The open branch picker and its subscription.
struct OpenPicker {
    picker: Entity<BranchPicker>,
    _subscription: Subscription,
}

/// The root view of the Argus window.
pub struct Workspace {
    focus_handle: FocusHandle,
    settings: UiSettings,
    settings_dirty: bool,
    fonts: InstalledFonts,
    session: Option<Entity<RepoSession>>,
    session_subscriptions: Vec<Subscription>,
    /// The changed files of the session's diff (left dock).
    file_tree: Option<Entity<FileTree>>,
    /// The file last opened from the tree; the diff viewer (#9) scrolls to
    /// it.
    open_file: Option<SharedString>,
    picker: Option<OpenPicker>,
    picker_closed: Option<(Side, Instant)>,
    /// Reopen the most recent repository once the list first loads.
    restore_last_repo: bool,
    recent: RecentRepos,
    sheet: Option<Sheet>,
    notice: Option<Notice>,
    save_task: Option<Task<()>>,
    open_task: Option<Task<()>>,
    recent_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// Builds the shell with the settings loaded at startup, installs the
    /// theme and starts loading the recent repositories.
    pub fn new(settings: UiSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        let subscriptions = vec![
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.settings.theme == ThemeMode::System {
                    this.apply_theme(window, cx);
                }
            }),
            cx.observe_global::<BackgroundJobs>(|_, cx| cx.notify()),
            cx.on_app_quit(|this, cx| {
                let flush = this.flush_settings(cx);
                async move {
                    if let Some(flush) = flush {
                        flush.await;
                    }
                }
            }),
        ];

        let mut this = Self {
            focus_handle,
            settings,
            settings_dirty: false,
            fonts: InstalledFonts::load(cx),
            session: None,
            session_subscriptions: Vec::new(),
            file_tree: None,
            open_file: None,
            picker: None,
            picker_closed: None,
            restore_last_repo: true,
            recent: RecentRepos::Loading,
            sheet: None,
            notice: None,
            save_task: None,
            open_task: None,
            recent_task: None,
            _subscriptions: subscriptions,
        };
        this.apply_theme(window, cx);
        this.reload_recent(cx);
        this
    }

    // ---- theme and settings -------------------------------------------------

    /// Rebuilds the [`Theme`] global from the settings and the window
    /// appearance, then repaints every window.
    fn apply_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fonts = ThemeFonts {
            ui_family: self
                .fonts
                .ui_family(self.settings.ui_font_family.as_deref()),
            ui_size: px(self.settings.ui_font_size),
            code_family: self
                .fonts
                .code_family(self.settings.code_font_family.as_deref()),
            code_size: px(self.settings.code_font_size),
        };
        let appearance = self.settings.theme.resolve(window.appearance());
        cx.set_global(Theme::new(appearance, fonts));
        cx.refresh_windows();
    }

    /// Changes the settings, applies them live and schedules a save.
    fn update_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut UiSettings),
    ) {
        let before = self.settings.clone();
        change(&mut self.settings);
        if self.settings == before {
            return;
        }
        let theme_changed = self.settings.theme != before.theme
            || self.settings.ui_font_family != before.ui_font_family
            || self.settings.code_font_family != before.code_font_family
            || self.settings.ui_font_size != before.ui_font_size
            || self.settings.code_font_size != before.code_font_size;
        if theme_changed {
            self.apply_theme(window, cx);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Debounced save: replacing the task cancels the previous pending write.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.settings_dirty = true;
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let Ok(Some(write)) = this.update(cx, |this, cx| this.flush_settings(cx)) else {
                return;
            };
            write.await;
        }));
    }

    /// Starts writing the settings on the tokio runtime if they changed since
    /// the last write; returns the pending write.
    fn flush_settings(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        if !self.settings_dirty {
            return None;
        }
        self.settings_dirty = false;
        let settings = self.settings.clone();
        let (store, runtime) = services(cx);
        let job = runtime.spawn(async move { settings.save(&store.settings()).await });
        Some(cx.background_executor().spawn(async move {
            match job.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "failed to save UI settings"),
                Err(error) => tracing::warn!(%error, "UI settings save task failed"),
            }
        }))
    }

    fn change_font_size(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.update_settings(window, cx, |settings| {
            settings.ui_font_size = clamp_font_size(settings.ui_font_size + delta, 0.0);
            settings.code_font_size = clamp_font_size(settings.code_font_size + delta, 0.0);
        });
    }

    // ---- repositories -------------------------------------------------------

    /// Reloads the recent repositories from the `repos` table.
    fn reload_recent(&mut self, cx: &mut Context<Self>) {
        let (store, runtime) = services(cx);
        let job = runtime.spawn(async move { store.repos().list().await });
        self.recent_task = Some(cx.spawn(async move |this, cx| {
            let recent = match job.await {
                Ok(Ok(repos)) => RecentRepos::Loaded(repos),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "failed to list recent repositories");
                    RecentRepos::Failed(error.to_string().into())
                }
                Err(error) => RecentRepos::Failed(error.to_string().into()),
            };
            this.update(cx, |this, cx| {
                // After a restart, come back to where the user left off: the
                // last repository, with its last base/head (#6).
                let restore = std::mem::take(&mut this.restore_last_repo);
                if let RecentRepos::Loaded(repos) = &recent
                    && restore
                    && this.session.is_none()
                    && this.open_task.is_none()
                    && let Some(last) = repos.first().filter(|repo| repo.last_opened_at.is_some())
                {
                    let path = last.path.clone();
                    this.open_path(path, cx);
                }
                this.recent = recent;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Opens the repository containing `path`, records it in `repos` and
    /// starts a session for it.
    fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let app = cx.global::<AppContext>();
        let (git, store) = (app.git.clone(), app.store.clone());
        let job = spawn_job(cx, open_repository(git, store, path));
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|error| Err(error.to_string()));
            this.update(cx, |this, cx| {
                this.open_task = None;
                match result {
                    Ok((record, repo)) => {
                        tracing::info!(path = %record.path.display(), "opened repository");
                        this.notice = None;
                        this.start_session(record, repo, cx);
                    }
                    Err(message) => {
                        tracing::warn!(%message, "failed to open repository");
                        this.notice = Some(Notice {
                            message: message.into(),
                            is_error: true,
                        });
                    }
                }
                this.reload_recent(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Replaces the open repository, cancelling every job of the old one.
    fn start_session(&mut self, record: Repo, repo: argus_git::Repo, cx: &mut Context<Self>) {
        self.close_picker(cx);
        if let Some(old) = self.session.take() {
            old.update(cx, |session, cx| session.shutdown(cx));
        }
        if let Some(old) = self.file_tree.take() {
            old.update(cx, |tree, cx| tree.shutdown(cx));
        }
        self.open_file = None;
        let session = cx.new(|cx| RepoSession::new(record, repo, cx));
        let flat = self.settings.file_tree.flat_list;
        let file_tree = cx.new(|cx| FileTree::new(flat, cx));
        self.session_subscriptions = vec![
            cx.observe(&session, |_, _, cx| cx.notify()),
            cx.observe(&file_tree, |_, _, cx| cx.notify()),
            cx.subscribe(
                &file_tree,
                |this, _, event: &FileTreeEvent, cx| match event {
                    FileTreeEvent::OpenFile { path, .. } => {
                        this.open_file = Some(path.clone());
                        cx.notify();
                    }
                    FileTreeEvent::OpenOutdated(item) => {
                        // The outdated-comment view arrives with #15.
                        tracing::debug!(item, "open outdated comment");
                    }
                    FileTreeEvent::ModeChanged { flat } => {
                        if this.settings.file_tree.flat_list != *flat {
                            this.settings.file_tree.flat_list = *flat;
                            this.schedule_save(cx);
                        }
                    }
                },
            ),
            cx.subscribe(
                &session,
                |this, session, event: &SessionEvent, cx| match event {
                    SessionEvent::Notice { message, is_error } => {
                        this.notice = Some(Notice {
                            message: message.clone(),
                            is_error: *is_error,
                        });
                        cx.notify();
                    }
                    SessionEvent::TargetChanged(target) => {
                        tracing::debug!(?target, "diff target changed");
                        let repo = session.read(cx).repo().clone();
                        if let Some(file_tree) = this.file_tree.clone() {
                            file_tree.update(cx, |tree, cx| {
                                tree.set_target(&repo, target.as_ref(), cx);
                            });
                        }
                    }
                },
            ),
        ];
        self.session = Some(session);
        self.file_tree = Some(file_tree);
        cx.notify();
    }

    // ---- branch pickers -----------------------------------------------------

    fn toggle_picker(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let open_side = self.picker.as_ref().map(|open| open.picker.read(cx).side());
        if open_side.is_some() {
            self.close_picker(cx);
            if open_side == Some(side) {
                window.focus(&self.focus_handle, cx);
                return;
            }
        }
        if let Some((closed_side, at)) = self.picker_closed.take()
            && closed_side == side
            && at.elapsed() < PICKER_REOPEN_GUARD
        {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let session = session.read(cx);
        let BranchList::Loaded(branches) = session.branches() else {
            return;
        };
        let (branches, selected) = (branches.clone(), session.selected(side).map(str::to_owned));
        let picker = cx.new(|cx| BranchPicker::new(side, branches, selected, cx));
        let subscription = cx.subscribe_in(
            &picker,
            window,
            |this, _, event: &PickerEvent, window, cx| {
                match event {
                    PickerEvent::Picked(side, name) => {
                        if let Some(session) = this.session.clone() {
                            let (side, name) = (*side, name.clone());
                            session.update(cx, |session, cx| session.pick(side, name, cx));
                        }
                        this.close_picker(cx);
                    }
                    PickerEvent::Dismissed => {
                        let side = this.picker.as_ref().map(|open| open.picker.read(cx).side());
                        this.close_picker(cx);
                        if let Some(side) = side {
                            this.picker_closed = Some((side, Instant::now()));
                        }
                    }
                }
                window.focus(&this.focus_handle, cx);
            },
        );
        window.focus(&picker.focus_handle(cx), cx);
        self.picker = Some(OpenPicker {
            picker,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            cx.notify();
        }
    }

    fn update_session(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut RepoSession, &mut Context<RepoSession>),
    ) {
        if let Some(session) = self.session.clone() {
            session.update(cx, f);
        }
    }

    // ---- actions ------------------------------------------------------------

    fn open_repository(&mut self, _: &OpenRepository, _: &mut Window, cx: &mut Context<Self>) {
        self.sheet = None;
        self.close_picker(cx);
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Repository".into()),
        });
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let path = match picked.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(error)) => {
                    let message = format!("Could not open the folder picker: {error}");
                    this.update(cx, |this, cx| {
                        this.notice = Some(Notice {
                            message: message.into(),
                            is_error: true,
                        });
                        cx.notify();
                    })
                    .ok();
                    None
                }
            };
            if let Some(path) = path {
                this.update(cx, |this, cx| this.open_path(path, cx)).ok();
            }
        }));
        cx.notify();
    }

    fn toggle_sheet(&mut self, sheet: Sheet, cx: &mut Context<Self>) {
        self.sheet = if self.sheet == Some(sheet) {
            None
        } else {
            Some(sheet)
        };
        cx.notify();
    }

    fn open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_sheet(Sheet::Settings, cx);
    }

    fn toggle_shortcuts(
        &mut self,
        _: &ToggleKeyboardShortcuts,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_sheet(Sheet::KeyboardShortcuts, cx);
    }

    fn dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        let tracking = self
            .session
            .as_ref()
            .is_some_and(|session| session.read(cx).tracking_prompt().is_some());
        if self.picker.is_some() {
            self.close_picker(cx);
            window.focus(&self.focus_handle, cx);
        } else if tracking {
            self.update_session(cx, |session, cx| session.cancel_tracking(cx));
        } else if self.sheet.take().is_some() {
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn toggle_file_tree(
        &mut self,
        _: &ToggleFileTree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_settings(window, cx, |settings| {
            settings.file_tree.visible = !settings.file_tree.visible;
        });
    }

    fn toggle_run_panel(
        &mut self,
        _: &ToggleRunPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_settings(window, cx, |settings| {
            settings.run_panel.visible = !settings.run_panel.visible;
        });
    }

    fn toggle_run_panel_position(
        &mut self,
        _: &ToggleRunPanelPosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_settings(window, cx, |settings| {
            settings.run_panel.position = settings.run_panel.position.toggled();
            settings.run_panel.visible = true;
        });
    }

    fn cycle_theme(&mut self, _: &CycleTheme, window: &mut Window, cx: &mut Context<Self>) {
        self.update_settings(window, cx, |settings| {
            settings.theme = settings.theme.next()
        });
    }

    fn increase_font_size(&mut self, _: &IncreaseFontSize, w: &mut Window, cx: &mut Context<Self>) {
        self.change_font_size(1.0, w, cx);
    }

    fn decrease_font_size(&mut self, _: &DecreaseFontSize, w: &mut Window, cx: &mut Context<Self>) {
        self.change_font_size(-1.0, w, cx);
    }

    fn reset_font_size(&mut self, _: &ResetFontSize, window: &mut Window, cx: &mut Context<Self>) {
        self.update_settings(window, cx, |settings| {
            settings.ui_font_size = DEFAULT_UI_FONT_SIZE;
            settings.code_font_size = DEFAULT_CODE_FONT_SIZE;
        });
    }

    fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        self.step_file(1, cx);
    }

    fn previous_file(&mut self, _: &PreviousFile, _: &mut Window, cx: &mut Context<Self>) {
        self.step_file(-1, cx);
    }

    /// `j` / `k`: the next or previous file of the tree, from anywhere.
    fn step_file(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(file_tree) = self.file_tree.clone() {
            file_tree.update(cx, |tree, cx| tree.select_adjacent_file(delta, cx));
        }
    }

    fn focus_file_filter(
        &mut self,
        _: &FocusFileFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file_tree) = self.file_tree.clone() else {
            return;
        };
        if !self.settings.file_tree.visible {
            self.update_settings(window, cx, |settings| settings.file_tree.visible = true);
        }
        let handle = file_tree.read(cx).filter_focus_handle().clone();
        window.focus(&handle, cx);
    }

    fn toggle_file_list_mode(
        &mut self,
        _: &ToggleFileListMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(file_tree) = self.file_tree.clone() {
            file_tree.update(cx, |tree, cx| tree.toggle_mode(cx));
        }
    }

    /// Placeholder handler for review actions whose views land in later
    /// issues (diff view #9, comments #14, runs #21, accept #24).
    fn not_wired_yet<A: Action>(&mut self, action: &A, _: &mut Window, _: &mut Context<Self>) {
        tracing::debug!(action = action.name(), "action not wired yet");
    }

    // ---- dock resizing ------------------------------------------------------

    fn resize_dock(
        &mut self,
        dock: DockResize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = window.viewport_size();
        let (width, height) = (viewport.width.as_f32(), viewport.height.as_f32());
        let run_panel = &self.settings.run_panel;
        let right_dock_width = if run_panel.visible && run_panel.position == RunPanelPosition::Right
        {
            run_panel.width
        } else {
            0.0
        };
        let left_dock_width = if self.settings.file_tree.visible {
            self.settings.file_tree.width
        } else {
            0.0
        };
        let x = position.x.as_f32();
        let y = position.y.as_f32();
        self.update_settings(window, cx, |settings| match dock {
            DockResize::FileTree => {
                let max = width - right_dock_width - MIN_CENTER_SIZE;
                settings.file_tree.width = clamp_size(x, max);
            }
            DockResize::RunPanel => match settings.run_panel.position {
                RunPanelPosition::Right => {
                    let max = width - left_dock_width - MIN_CENTER_SIZE;
                    settings.run_panel.width = clamp_size(width - x, max);
                }
                RunPanelPosition::Bottom => {
                    let bottom = height - STATUS_BAR_HEIGHT.as_f32();
                    let max = bottom - TITLE_BAR_HEIGHT.as_f32() - MIN_CENTER_SIZE;
                    settings.run_panel.height = clamp_size(bottom - y, max);
                }
            },
        });
    }

    fn reset_dock_size(&mut self, dock: DockResize, window: &mut Window, cx: &mut Context<Self>) {
        self.update_settings(window, cx, |settings| match dock {
            DockResize::FileTree => settings.file_tree.width = DEFAULT_FILE_TREE_WIDTH,
            DockResize::RunPanel => match settings.run_panel.position {
                RunPanelPosition::Right => settings.run_panel.width = DEFAULT_RUN_PANEL_WIDTH,
                RunPanelPosition::Bottom => settings.run_panel.height = DEFAULT_RUN_PANEL_HEIGHT,
            },
        });
    }

    /// The invisible grab strip on a dock's inner edge. Drag to resize,
    /// double-click to reset.
    fn resize_handle(
        &self,
        dock: DockResize,
        vertical: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accent = cx.theme().colors.accent;
        let id = match dock {
            DockResize::FileTree => "file-tree-resize",
            DockResize::RunPanel => "run-panel-resize",
        };
        let handle = div()
            .id(id)
            .absolute()
            .hover(move |style| style.bg(accent.opacity(0.6)))
            .on_drag(dock, |_, _, _, cx| cx.new(|_| EmptyView))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    if event.click_count() == 2 {
                        this.reset_dock_size(dock, window, cx);
                    }
                }),
            );
        let offset = -(RESIZE_HANDLE_SIZE / 2.0);
        let handle = match (dock, vertical) {
            (DockResize::FileTree, _) => handle
                .top_0()
                .bottom_0()
                .right(offset)
                .w(RESIZE_HANDLE_SIZE)
                .cursor_col_resize(),
            (DockResize::RunPanel, false) => handle
                .top_0()
                .bottom_0()
                .left(offset)
                .w(RESIZE_HANDLE_SIZE)
                .cursor_col_resize(),
            (DockResize::RunPanel, true) => handle
                .left_0()
                .right_0()
                .top(offset)
                .h(RESIZE_HANDLE_SIZE)
                .cursor_row_resize(),
        };
        handle.into_any_element()
    }

    // ---- rendering ----------------------------------------------------------

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let session = self.session.as_ref().map(|session| session.read(cx));
        let repo_label: SharedString = session
            .map(|session| SharedString::from(session.record().name.clone()))
            .unwrap_or_else(|| "Open Repository…".into());

        let branch_controls = session.map(|session| {
            let loaded = matches!(session.branches(), BranchList::Loaded(_));
            div()
                .flex()
                .items_center()
                .gap_0p5()
                .min_w_0()
                .child(self.render_branch_button(Side::Base, session, loaded, cx))
                .child(if loaded {
                    button("title-swap", "⇄", cx)
                        .text_color(colors.text_muted)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.close_picker(cx);
                            this.update_session(cx, |session, cx| session.swap(cx));
                        }))
                } else {
                    disabled_button("title-swap", "⇄", cx)
                })
                .child(self.render_branch_button(Side::Head, session, loaded, cx))
        });

        let fetch = match session {
            Some(session) if session.is_fetching() => {
                disabled_button("title-fetch", "Fetching…", cx)
            }
            Some(session) if session.has_remotes() => {
                button("title-fetch", "Fetch", cx).on_click(cx.listener(|this, _, _, cx| {
                    this.update_session(cx, |session, cx| session.fetch(cx));
                }))
            }
            _ => disabled_button("title-fetch", "Fetch", cx),
        };
        // Push itself arrives with #25; the counts are live already.
        let push_label = match session.and_then(|session| session.upstream()) {
            Some(status) => format!("Push ↑{} ↓{}", status.ahead, status.behind),
            None => "Push".to_owned(),
        };

        div()
            .id("title-bar")
            .window_control_area(WindowControlArea::Drag)
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .h(TITLE_BAR_HEIGHT)
            .pl(TRAFFIC_LIGHTS_WIDTH)
            .pr_2()
            .gap_2()
            .bg(colors.title_bar)
            .border_b_1()
            .border_color(colors.border)
            .on_click(|event, window, _| {
                if event.click_count() == 2 && cfg!(target_os = "macos") {
                    window.titlebar_double_click();
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .min_w_0()
                    .child(
                        button("title-repo", repo_label, cx)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(OpenRepository.boxed_clone(), cx)
                            }),
                    )
                    .children(branch_controls),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .child(fetch)
                    .child(disabled_button("title-push", push_label, cx)),
            )
    }

    /// A `base ▾` / `head ▾` button, with its picker popover when open.
    fn render_branch_button(
        &self,
        side: Side,
        session: &RepoSession,
        enabled: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = &cx.theme().colors;
        let id = match side {
            Side::Base => "title-base",
            Side::Head => "title-head",
        };
        let (label, placeholder): (SharedString, bool) = match session.selected(side) {
            Some(name) => (format!("{name} ▾").into(), false),
            None => ("pick ▾".into(), true),
        };
        let picker = self
            .picker
            .as_ref()
            .filter(|open| open.picker.read(cx).side() == side)
            .map(|open| open.picker.clone());
        let trigger = if enabled {
            button(id, label, cx)
                .max_w(px(260.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .when(placeholder, |button| button.text_color(colors.text_muted))
                .when(picker.is_some(), |button| {
                    button.bg(colors.element_selected)
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.toggle_picker(side, window, cx);
                }))
        } else {
            disabled_button(id, label, cx)
        };
        div()
            .relative()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_placeholder)
                            .child(side.label()),
                    )
                    .child(trigger),
            )
            .when_some(picker, |slot, picker| {
                slot.child(
                    deferred(div().absolute().top(px(30.0)).left_0().child(picker))
                        .with_priority(1),
                )
            })
    }

    fn render_file_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let comparison = self
            .session
            .as_ref()
            .map(|session| session.read(cx).comparison());
        // While a refresh resolves, the previous list stays up (with its
        // expanded directories) instead of flashing a placeholder.
        let has_list = self
            .file_tree
            .as_ref()
            .is_some_and(|tree| tree.read(cx).file_list().is_some());
        let body: AnyElement = match (comparison, self.file_tree.clone()) {
            (Some(Comparison::Ready(_)), Some(tree)) => tree.into_any_element(),
            (Some(Comparison::Resolving), Some(tree)) if has_list => tree.into_any_element(),
            (comparison, _) => {
                let text: SharedString = match comparison {
                    None => "No repository open.".into(),
                    Some(Comparison::Ready(_)) | Some(Comparison::Resolving) => {
                        "Resolving the comparison…".into()
                    }
                    Some(Comparison::Incomplete(message)) => message.clone(),
                    Some(Comparison::Invalid(_)) => "No diff to list.".into(),
                };
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(panel_header("Files", cx))
                    .child(placeholder(text, cx))
                    .into_any_element()
            }
        };
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .h_full()
            .w(px(self.settings.file_tree.width))
            .bg(colors.surface)
            .border_r_1()
            .border_color(colors.border)
            .child(body)
            .child(self.resize_handle(DockResize::FileTree, false, cx))
    }

    fn render_run_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let bottom = self.settings.run_panel.position == RunPanelPosition::Bottom;
        let panel = div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .bg(colors.surface)
            .border_color(colors.border)
            .child(panel_header("Runs", cx))
            .child(placeholder(
                "No runs yet. Comment on a line and run it with Claude.",
                cx,
            ))
            .child(self.resize_handle(DockResize::RunPanel, bottom, cx));
        if bottom {
            panel
                .w_full()
                .h(px(self.settings.run_panel.height))
                .border_t_1()
        } else {
            panel
                .h_full()
                .w(px(self.settings.run_panel.width))
                .border_l_1()
        }
    }

    fn render_center(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let tab_title: SharedString = match self
            .session
            .as_ref()
            .map(|session| session.read(cx).comparison())
        {
            Some(Comparison::Ready(target)) => format!("{}...{}", target.base, target.head).into(),
            Some(_) => "Diff".into(),
            None => "Welcome".into(),
        };
        let content = match self.session.clone() {
            Some(session) => self.render_comparison(session.read(cx), cx),
            None => self.render_empty_state(cx).into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(colors.background)
            .child(
                // Tab bar: the main diff tab; proposal tabs join it later.
                div()
                    .flex()
                    .flex_none()
                    .h(px(32.0))
                    .bg(colors.surface)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .relative()
                            .flex()
                            .items_center()
                            .h_full()
                            .px_3()
                            .bg(colors.background)
                            .border_r_1()
                            .border_color(colors.border)
                            .text_sm()
                            .text_color(colors.text)
                            .child(tab_title)
                            // Active-tab marker.
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .right_0()
                                    .h(px(2.0))
                                    .bg(colors.accent),
                            ),
                    ),
            )
            .child(content)
    }

    /// The center of an open repository: the resolved comparison, an inline
    /// error when the pair cannot be diffed, or what to pick next.
    fn render_comparison(&self, session: &RepoSession, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let path = display_path(&session.record().path);
        match session.comparison() {
            Comparison::Ready(target) => {
                self.render_diff_placeholder(target, cx).into_any_element()
            }
            Comparison::Invalid(message) => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .p_3()
                .gap_2()
                .child(div().text_sm().text_color(colors.text_muted).child(path))
                .child(
                    div()
                        .id("comparison-error")
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.error.opacity(0.6))
                        .bg(colors.error.opacity(0.08))
                        .text_sm()
                        .text_color(colors.error)
                        .child(message.clone()),
                )
                .into_any_element(),
            Comparison::Resolving => {
                placeholder("Computing the merge-base…", cx).into_any_element()
            }
            Comparison::Incomplete(message) => placeholder(message.clone(), cx).into_any_element(),
        }
    }

    fn render_diff_placeholder(&self, target: &DiffTarget, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        // (sign, text before the changed word, changed word, text after).
        let line = |sign: &'static str, parts: [&'static str; 3], bg, word_bg, fg| {
            div()
                .flex()
                .px_3()
                .bg(bg)
                .child(div().w(px(16.0)).text_color(fg).child(sign))
                .child(parts[0])
                .child(div().bg(word_bg).child(parts[1]))
                .child(parts[2])
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child(format!(
                        "{}...{} · merge-base {} · head {} · the diff viewer arrives with #9.",
                        target.base,
                        target.head,
                        target.merge_base.short(8),
                        target.head_tip.short(8),
                    )),
            )
            .when_some(self.open_file.clone(), |column, path| {
                column.child(
                    div()
                        .px_3()
                        .pb_2()
                        .text_sm()
                        .text_color(colors.text)
                        .child(format!("Selected file: {path}")),
                )
            })
            .child(
                // A preview of the diff colors in the code font, so the theme
                // can be checked before the viewer lands.
                div()
                    .flex()
                    .flex_col()
                    .mx_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .font_family(theme.fonts.code_family.clone())
                    .text_size(theme.fonts.code_size)
                    .text_color(colors.text)
                    .child(line(
                        " ",
                        ["fn review(diff: &Diff) -> Verdict {", "", ""],
                        colors.background,
                        colors.background,
                        colors.text_muted,
                    ))
                    .child(line(
                        "-",
                        ["    Verdict::", "approve()", ""],
                        colors.diff_removed_background,
                        colors.diff_removed_word,
                        colors.diff_removed_text,
                    ))
                    .child(line(
                        "+",
                        ["    Verdict::", "comment(diff.hunks())", ""],
                        colors.diff_added_background,
                        colors.diff_added_word,
                        colors.diff_added_text,
                    ))
                    .child(line(
                        " ",
                        ["}", "", ""],
                        colors.background,
                        colors.background,
                        colors.text_muted,
                    )),
            )
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let recent: AnyElement =
            match &self.recent {
                RecentRepos::Loading => div()
                    .text_sm()
                    .text_color(colors.text_placeholder)
                    .child("Loading recent repositories…")
                    .into_any_element(),
                RecentRepos::Failed(message) => div()
                    .text_sm()
                    .text_color(colors.error)
                    .child(format!("Could not load recent repositories: {message}"))
                    .into_any_element(),
                RecentRepos::Loaded(repos) if repos.is_empty() => div()
                    .text_sm()
                    .text_color(colors.text_placeholder)
                    .child("No recent repositories.")
                    .into_any_element(),
                RecentRepos::Loaded(repos) => div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .children(repos.iter().take(RECENT_REPOS_SHOWN).enumerate().map(
                        |(index, repo)| {
                            let path = repo.path.clone();
                            div()
                                .id(("recent-repo", index))
                                .flex()
                                .flex_col()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|style| style.bg(colors.element_hover))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_path(path.clone(), cx)
                                }))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(colors.text)
                                        .child(repo.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(colors.text_muted)
                                        .child(display_path(&repo.path)),
                                )
                        },
                    ))
                    .into_any_element(),
            };

        div()
            .id("empty-state")
            .flex()
            .flex_1()
            .min_h_0()
            .items_center()
            .justify_center()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .w(px(420.0))
                    .p_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child("Argus"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(colors.text_muted)
                                    .child("Review a branch, comment on lines, and let Claude propose the fixes."),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(primary_button("open-repo", "Open a repository", cx).on_click(
                                |_, window, cx| {
                                    window.dispatch_action(OpenRepository.boxed_clone(), cx)
                                },
                            ))
                            .child(kbd(shortcut_text::<OpenRepository>(), cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.text_muted)
                                    .child("RECENT"),
                            )
                            .child(recent),
                    ),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let jobs = cx.global::<BackgroundJobs>().clone();
        let job_indicator = if jobs.is_busy() {
            div()
                .flex()
                .items_center()
                .gap_1()
                .text_color(colors.text)
                .child(div().size(px(6.0)).rounded_full().bg(colors.accent))
                .child(jobs.labels().join(" · "))
        } else {
            div().text_color(colors.text_placeholder).child("Idle")
        };
        let notice = self.notice.as_ref().map(|notice| {
            div()
                .id("status-notice")
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .cursor_pointer()
                .text_color(if notice.is_error {
                    colors.error
                } else {
                    colors.warning
                })
                .child(notice.message.clone())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.notice = None;
                    cx.notify();
                }))
        });

        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap_2()
            .h(STATUS_BAR_HEIGHT)
            .px_1()
            .bg(colors.status_bar)
            .border_t_1()
            .border_color(colors.border)
            .text_xs()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .min_w_0()
                    .child(
                        toggle_button("status-files", "Files", self.settings.file_tree.visible, cx)
                            .h(px(20.0))
                            .text_xs()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(ToggleFileTree.boxed_clone(), cx)
                            }),
                    )
                    .children(notice),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .child(job_indicator)
                    .child(
                        button("status-shortcuts", "Shortcuts", cx)
                            .h(px(20.0))
                            .text_xs()
                            .text_color(colors.text_muted)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(ToggleKeyboardShortcuts.boxed_clone(), cx)
                            }),
                    )
                    .child(
                        toggle_button("status-runs", "Runs", self.settings.run_panel.visible, cx)
                            .h(px(20.0))
                            .text_xs()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(ToggleRunPanel.boxed_clone(), cx)
                            }),
                    ),
            )
    }

    fn render_sheet(&self, sheet: Sheet, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let (title, body) = match sheet {
            Sheet::KeyboardShortcuts => (
                "Keyboard Shortcuts",
                self.render_shortcuts(cx).into_any_element(),
            ),
            Sheet::Settings => ("Settings", self.render_settings(cx).into_any_element()),
        };
        div()
            .id("sheet-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(64.0))
            .bg(colors.overlay)
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.sheet = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("sheet")
                    .flex()
                    .flex_col()
                    .w(px(match sheet {
                        Sheet::KeyboardShortcuts => 720.0,
                        Sheet::Settings => 520.0,
                    }))
                    .max_h(px(600.0))
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_between()
                            .h(px(40.0))
                            .px_4()
                            .border_b_1()
                            .border_color(colors.border_variant)
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(title),
                            )
                            .child(kbd(shortcut_text::<Dismiss>(), cx)),
                    )
                    .child(
                        div()
                            .id("sheet-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_4()
                            .child(body),
                    ),
            )
    }

    /// Confirmation for a remote-only head: create a local tracking branch,
    /// or cancel and keep the current selection.
    fn render_tracking_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.session.as_ref()?.read(cx);
        let prompt = session.tracking_prompt()?;
        let colors = cx.theme().colors.clone();
        let code = |text: &str| {
            div()
                .px_1()
                .rounded_sm()
                .bg(colors.element_hover)
                .font_family(cx.theme().fonts.code_family.clone())
                .child(text.to_owned())
        };
        let (title, explanation, confirm_label) = if prompt.local_exists {
            (
                "Use the local branch?",
                "already exists locally. Argus reviews the local branch, since Accept and Push \
                 need one.",
                "Use local branch",
            )
        } else {
            (
                "Create a local tracking branch?",
                "is a remote-tracking branch. Accept and Push need a local branch, so Argus can \
                 create one that tracks it. Your checkout is not touched.",
                "Create branch",
            )
        };
        let command = format!(
            "git branch --track {} {}",
            prompt.local_name, prompt.remote_branch
        );
        let confirm = if prompt.creating {
            disabled_button("tracking-confirm", "Creating…", cx).into_any_element()
        } else {
            primary_button("tracking-confirm", confirm_label, cx)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_session(cx, |session, cx| session.confirm_tracking(cx));
                }))
                .into_any_element()
        };
        Some(
            div()
                .id("tracking-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(96.0))
                .bg(colors.overlay)
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.update_session(cx, |session, cx| session.cancel_tracking(cx));
                    }),
                )
                .child(
                    div()
                        .id("tracking-dialog")
                        .flex()
                        .flex_col()
                        .gap_3()
                        .w(px(460.0))
                        .p_4()
                        .rounded_lg()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.elevated_surface)
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(colors.text)
                                .child(title),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_1()
                                .text_sm()
                                .text_color(colors.text_muted)
                                .child(code(if prompt.local_exists {
                                    &prompt.local_name
                                } else {
                                    &prompt.remote_branch
                                }))
                                .child(explanation),
                        )
                        .when(!prompt.local_exists, |dialog| {
                            dialog.child(
                                div()
                                    .text_xs()
                                    .text_color(colors.text_muted)
                                    .child(code(&command)),
                            )
                        })
                        .when_some(prompt.error.clone(), |dialog, error| {
                            dialog.child(div().text_sm().text_color(colors.error).child(error))
                        })
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(button("tracking-cancel", "Cancel", cx).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.update_session(cx, |session, cx| {
                                            session.cancel_tracking(cx)
                                        });
                                    }),
                                ))
                                .child(confirm),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let group = |name: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(name.to_uppercase()),
                )
                .children(
                    SHORTCUTS
                        .iter()
                        .filter(|shortcut| shortcut.group == name)
                        .map(|shortcut| {
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .py_0p5()
                                .text_sm()
                                .text_color(colors.text)
                                .child(shortcut.description)
                                .child(kbd(shortcut.display_keys(), cx))
                        }),
                )
        };
        let (left, right) = SHORTCUT_GROUPS.split_at(SHORTCUT_GROUPS.len().div_ceil(2));
        let column = |groups: &[&'static str]| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_4()
                .children(groups.iter().map(|name| group(name)))
        };
        div()
            .flex()
            .items_start()
            .gap_6()
            .child(column(left))
            .child(column(right))
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let settings = &self.settings;
        let row = |label: &'static str, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .py_1()
                .text_sm()
                .child(div().text_color(colors.text).child(label))
                .child(control)
        };
        let segmented = |children: Vec<AnyElement>| {
            div()
                .flex()
                .items_center()
                .gap_0p5()
                .p_0p5()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .children(children)
                .into_any_element()
        };
        let stepper = |id: &'static str, value: f32, apply: fn(&mut UiSettings, f32)| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(button((id, 0usize), "−", cx).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.update_settings(window, cx, |settings| apply(settings, -1.0))
                    },
                )))
                .child(
                    div()
                        .w(px(48.0))
                        .text_center()
                        .text_color(colors.text)
                        .child(format!("{value:.0} px")),
                )
                .child(button((id, 1usize), "+", cx).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.update_settings(window, cx, |settings| apply(settings, 1.0))
                    },
                )))
                .into_any_element()
        };
        let muted = |text: SharedString| {
            div()
                .text_color(colors.text_muted)
                .child(text)
                .into_any_element()
        };

        let theme_buttons = ThemeMode::ALL
            .iter()
            .map(|mode| {
                let mode = *mode;
                toggle_button(
                    ("theme-mode", mode as usize),
                    mode.label(),
                    settings.theme == mode,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_settings(window, cx, |settings| settings.theme = mode)
                }))
                .into_any_element()
            })
            .collect();
        let position_buttons = [RunPanelPosition::Right, RunPanelPosition::Bottom]
            .into_iter()
            .map(|position| {
                toggle_button(
                    ("run-panel-position", position as usize),
                    position.label(),
                    settings.run_panel.position == position,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_settings(window, cx, |settings| {
                        settings.run_panel.position = position;
                    })
                }))
                .into_any_element()
            })
            .collect();
        let visibility = |id: &'static str, visible: bool, action: Box<dyn Action>| {
            toggle_button(id, if visible { "Shown" } else { "Hidden" }, visible, cx)
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(row("Theme", segmented(theme_buttons)))
            .child(row("Active theme", muted(theme.name.clone())))
            .child(row(
                "UI font size",
                stepper("ui-font-size", settings.ui_font_size, |settings, delta| {
                    settings.ui_font_size = clamp_font_size(settings.ui_font_size + delta, 0.0);
                }),
            ))
            .child(row(
                "Code font size",
                stepper(
                    "code-font-size",
                    settings.code_font_size,
                    |settings, delta| {
                        settings.code_font_size =
                            clamp_font_size(settings.code_font_size + delta, 0.0);
                    },
                ),
            ))
            .child(row("UI font", muted(theme.fonts.ui_family.clone())))
            .child(row("Code font", muted(theme.fonts.code_family.clone())))
            .child(row(
                "File tree",
                visibility(
                    "file-tree-visible",
                    settings.file_tree.visible,
                    ToggleFileTree.boxed_clone(),
                ),
            ))
            .child(row(
                "Run panel",
                visibility(
                    "run-panel-visible",
                    settings.run_panel.visible,
                    ToggleRunPanel.boxed_clone(),
                ),
            ))
            .child(row("Run panel position", segmented(position_buttons)))
            .child(row(
                "Dock sizes",
                button("reset-layout", "Reset", cx)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.update_settings(window, cx, |settings| {
                            settings.file_tree.width = DEFAULT_FILE_TREE_WIDTH;
                            settings.run_panel.width = DEFAULT_RUN_PANEL_WIDTH;
                            settings.run_panel.height = DEFAULT_RUN_PANEL_HEIGHT;
                        })
                    }))
                    .into_any_element(),
            ))
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // Rem-based spacing and text sizes scale with the UI font size, as in
        // Zed.
        window.set_rem_size(theme.fonts.ui_size);

        let run_panel = &self.settings.run_panel;
        let run_panel_bottom = run_panel.visible && run_panel.position == RunPanelPosition::Bottom;
        let run_panel_right = run_panel.visible && run_panel.position == RunPanelPosition::Right;

        let main_column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_center(cx))
            .when(run_panel_bottom, |column| {
                column.child(self.render_run_panel(cx))
            });

        let body = div()
            .flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .when(self.settings.file_tree.visible, |body| {
                body.child(self.render_file_tree(cx))
            })
            .child(main_column)
            .when(run_panel_right, |body| {
                body.child(self.render_run_panel(cx))
            });

        div()
            .id("workspace")
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(theme.colors.background)
            .text_color(theme.colors.text)
            .font_family(theme.fonts.ui_family.clone())
            .text_size(theme.fonts.ui_size)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // Keep keyboard shortcuts working after clicks on
                    // non-focusable chrome.
                    if window.focused(cx).is_none() {
                        window.focus(&this.focus_handle, cx);
                    }
                }),
            )
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DockResize>, window, cx| {
                    let dock = *event.drag(cx);
                    this.resize_dock(dock, event.event.position, window, cx);
                }),
            )
            .on_action(cx.listener(Self::open_repository))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::toggle_shortcuts))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::toggle_file_tree))
            .on_action(cx.listener(Self::toggle_run_panel))
            .on_action(cx.listener(Self::toggle_run_panel_position))
            .on_action(cx.listener(Self::cycle_theme))
            .on_action(cx.listener(Self::increase_font_size))
            .on_action(cx.listener(Self::decrease_font_size))
            .on_action(cx.listener(Self::reset_font_size))
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::previous_file))
            .on_action(cx.listener(Self::focus_file_filter))
            .on_action(cx.listener(Self::toggle_file_list_mode))
            .on_action(cx.listener(Self::not_wired_yet::<NextHunk>))
            .on_action(cx.listener(Self::not_wired_yet::<PreviousHunk>))
            .on_action(cx.listener(Self::not_wired_yet::<Comment>))
            .on_action(cx.listener(Self::not_wired_yet::<Run>))
            .on_action(cx.listener(Self::not_wired_yet::<Accept>))
            .child(self.render_title_bar(cx))
            .child(body)
            .child(self.render_status_bar(cx))
            .when_some(self.sheet, |root, sheet| {
                root.child(self.render_sheet(sheet, cx))
            })
            .children(self.render_tracking_dialog(cx))
    }
}

/// The store and runtime handles from the [`AppContext`] global.
fn services(cx: &gpui::App) -> (StorageService, tokio::runtime::Handle) {
    let app = cx.global::<AppContext>();
    (app.store.clone(), app.runtime.clone())
}

/// Clamps a dock size to `[MIN_DOCK_SIZE, max]`, preferring the minimum when
/// the window is too small for both.
fn clamp_size(size: f32, max: f32) -> f32 {
    size.min(max).max(MIN_DOCK_SIZE).round()
}

/// Formatted keystrokes of the shortcut bound to action `A`, for inline hints.
fn shortcut_text<A: Action>() -> String {
    SHORTCUTS
        .iter()
        .find(|shortcut| shortcut.binding().action().as_any().is::<A>())
        .map(|shortcut| shortcut.display_keys())
        .unwrap_or_default()
}

/// The path with the home directory shortened to `~`.
fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}
