//! The Zed-style app shell: title bar, file-tree dock, center tabs, run-panel
//! dock, status bar, and the settings and keyboard-shortcuts sheets.
//!
//! The viewer (#7/#8), comments and runs (#21) plug their views into the slots
//! rendered here. Settings and the repository list are read and written on the
//! tokio runtime; this view only receives the results.

use std::path::{Path, PathBuf};
use std::time::Duration;

use argus_store::{Repo, StorageService};
use gpui::{
    Action, AnyElement, Context, DragMoveEvent, EmptyView, FocusHandle, Focusable, MouseButton,
    PathPromptOptions, Pixels, Point, SharedString, Subscription, Task, Window, WindowControlArea,
    div, prelude::*, px,
};

use crate::AppContext;
use crate::components::{
    button, disabled_button, kbd, panel_header, placeholder, primary_button, toggle_button,
};
use crate::fonts::InstalledFonts;
use crate::jobs::BackgroundJobs;
use crate::keymap::{
    Accept, Comment, CycleTheme, DecreaseFontSize, Dismiss, IncreaseFontSize, NextFile, NextHunk,
    OpenRepository, OpenSettings, PreviousFile, PreviousHunk, ResetFontSize, Run, SHORTCUT_GROUPS,
    SHORTCUTS, ToggleFileTree, ToggleKeyboardShortcuts, ToggleRunPanel, ToggleRunPanelPosition,
    WORKSPACE_CONTEXT,
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

/// Why a repository could not be opened.
#[derive(Debug, thiserror::Error)]
enum OpenRepoError {
    #[error("{0} is not a git repository")]
    NotARepository(String),
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] argus_store::StoreError),
}

/// The root view of the Argus window.
pub struct Workspace {
    focus_handle: FocusHandle,
    settings: UiSettings,
    settings_dirty: bool,
    fonts: InstalledFonts,
    repo: Option<Repo>,
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
            repo: None,
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
                this.recent = recent;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Records the repository at `path` and shows it in the shell.
    fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let (store, runtime) = services(cx);
        let job = runtime.spawn(async move { register_repository(&store, path).await });
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let result = match job.await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(repo) => {
                        tracing::info!(path = %repo.path.display(), "opened repository");
                        this.notice = None;
                        this.repo = Some(repo);
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

    // ---- actions ------------------------------------------------------------

    fn open_repository(&mut self, _: &OpenRepository, _: &mut Window, cx: &mut Context<Self>) {
        self.sheet = None;
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

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.sheet.take().is_some() {
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

    /// Placeholder handler for review actions whose views land in later
    /// issues (file tree #8, diff view #7, comments, runs #21, accept).
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
        let repo_label: SharedString = self
            .repo
            .as_ref()
            .map(|repo| SharedString::from(repo.name.clone()))
            .unwrap_or_else(|| "Open Repository…".into());

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
                    // Branch picker slots, wired by #6.
                    .when(self.repo.is_some(), |row| {
                        row.child(disabled_button("title-base", "base ▾", cx))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(colors.text_placeholder)
                                    .child("…"),
                            )
                            .child(disabled_button("title-head", "head ▾", cx))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    // Fetch (#6) and Push (#25) slots.
                    .child(disabled_button("title-fetch", "Fetch", cx))
                    .child(disabled_button("title-push", "Push ↑0 ↓0", cx)),
            )
    }

    fn render_file_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let body = if self.repo.is_some() {
            placeholder("Pick a base and head branch to list the changed files.", cx)
        } else {
            placeholder("No repository open.", cx)
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
            .child(panel_header("Files", cx))
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
        let content = match &self.repo {
            Some(repo) => self.render_diff_placeholder(repo, cx).into_any_element(),
            None => self.render_empty_state(cx).into_any_element(),
        };
        let tab_title: SharedString = if self.repo.is_some() {
            "Diff".into()
        } else {
            "Welcome".into()
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

    fn render_diff_placeholder(&self, repo: &Repo, cx: &mut Context<Self>) -> impl IntoElement {
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
                        "{} — pick a base and head branch in the title bar to load the diff.",
                        repo.path.display()
                    )),
            )
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
            .on_action(cx.listener(Self::not_wired_yet::<NextFile>))
            .on_action(cx.listener(Self::not_wired_yet::<PreviousFile>))
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

/// Validates that `path` is a git repository and records it in `repos`.
/// Runs on the tokio runtime.
async fn register_repository(store: &StorageService, path: PathBuf) -> Result<Repo, OpenRepoError> {
    let io_error = |source| OpenRepoError::Io {
        path: path.display().to_string(),
        source,
    };
    let path = tokio::fs::canonicalize(&path).await.map_err(io_error)?;
    let has_git_dir = tokio::fs::try_exists(path.join(".git"))
        .await
        .map_err(io_error)?;
    let is_bare = tokio::fs::try_exists(path.join("HEAD"))
        .await
        .map_err(io_error)?
        && tokio::fs::try_exists(path.join("objects"))
            .await
            .map_err(io_error)?;
    if !has_git_dir && !is_bare {
        return Err(OpenRepoError::NotARepository(path.display().to_string()));
    }

    let repos = store.repos();
    let id = repos.upsert(&path).await?;
    repos.touch_last_opened(id).await?;
    repos.get(id).await?.ok_or_else(|| {
        OpenRepoError::Store(argus_store::StoreError::NotFound {
            entity: "repo",
            id: id.to_string(),
        })
    })
}
