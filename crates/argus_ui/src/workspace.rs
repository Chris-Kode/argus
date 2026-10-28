//! The Zed-style app shell: title bar, file-tree dock, center tabs, run-panel
//! dock, status bar, and the settings and keyboard-shortcuts sheets.
//!
//! The open repository and its base/head selection live in a [`RepoSession`];
//! the title bar hosts its branch pickers, swap and Fetch buttons. Each
//! resolved comparison gets a [`DiffView`] in the center. The left dock has
//! two tabs: the file tree (#8), with comment-count badges, and the comment
//! list (#14). The run panel (#21) lives in the right or bottom dock: the
//! diff's Run menu and `r` start runs through it, and its rows jump back to
//! their comments. "View proposal" on a `Proposed` comment opens a
//! [`ProposalView`] tab next to the main diff tab (#22); the navigation keys
//! follow the active tab. Accept goes through the [`AcceptFlow`] (#24): its
//! dialogs overlay the shell, and each landed commit refreshes the branches,
//! so the diff, the comment anchors and the Push counts follow. The title
//! bar's Push button goes through the [`PushFlow`] (#25): it is disabled
//! while an Accept is in flight, offers `git push -u` when the head has no
//! upstream, and the head's upstream is fetched in the background every
//! minute (a setting) so its counts stay current. A conflict of an Accept
//! or of the pull before Push opens the [`ConflictDialog`] (#26); its
//! resolve run's proposal opens in a tab when the run ends, and accepting
//! a pull-before-push resolution continues the Push. A run that waits for
//! the reviewer (a tool permission its `auto` mode does not settle, or an
//! `AskUserQuestion`) opens the [`PermissionDialog`]; Later hides it and the
//! status bar's "waiting for you" counter brings it back. The title bar's
//! Explain button (or `e`) opens the [`ExplainDialog`] for the main diff,
//! or for the lines selected in it; its caret picks the model and effort.
//! A click on a drawn Mermaid diagram opens the [`DiagramViewer`] over
//! everything but the permission dialog.
//! Settings, the
//! repository list and git are read and written on the tokio runtime; this
//! view only receives the results.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use argus_diff::layout::ViewMode;
use argus_diff::review::{CommentChange, CommentStatus};
use argus_runs::{ClaudeStatus, ExplainRequest, ExplainScope, PermissionRequest};
use argus_store::{Repo, StorageService};
use gpui::{
    Action, AnyElement, Context, DragMoveEvent, EmptyView, Entity, FocusHandle, Focusable,
    MouseButton, PathPromptOptions, Pixels, Point, SharedString, Subscription, Task, Window,
    WindowControlArea, deferred, div, prelude::*, px,
};

use crate::accept::{AcceptFlow, AcceptFlowEvent};
use crate::branch_picker::{BranchPicker, PickerEvent};
use crate::claude_settings::ClaudeSettingsForm;
use crate::claude_status::observe_claude_status;
use crate::comment_list::{CommentList, CommentListEvent};
use crate::comments::{ReviewComments, ReviewCommentsEvent};
use crate::components::{
    accent_button_with_icon, button, claude_activity, disabled_button,
    disabled_ghost_button_with_icon, disabled_icon_button, icon_button, kbd, panel_header,
    placeholder, primary_button, primary_button_with_icon, radius, section_header, text_tooltip,
    toggle_button, toggle_icon_button,
};
use crate::conflict::{ConflictDialog, ConflictDialogEvent, ConflictSource};
use crate::diagram_viewer::{DiagramViewer, DiagramViewerEvent};
use crate::diff_view::{DiffView, DiffViewEvent, DisplayOptions, LineSelection};
use crate::explain::{self, ExplainDialog, ExplainDialogEvent};
use crate::file_tree::{FileMarker, FileTree, FileTreeEvent};
use crate::followup;
use crate::fonts::InstalledFonts;
use crate::hidden_files::{self, HiddenFiles};
use crate::icons::{IconName, eye_mark, icon};
use crate::jobs::BackgroundJobs;
use crate::keymap::{
    Accept, AdvanceCommentStatus, CheckForUpdates, Comment, CycleTheme, DecreaseFontSize, Dismiss,
    Explain, FocusFileFilter, IncreaseFontSize, NextComment, NextFile, NextHunk, OpenRecent1,
    OpenRecent2, OpenRecent3, OpenRecent4, OpenRecent5, OpenRecent6, OpenRecent7, OpenRecent8,
    OpenRecent9, OpenRepository, OpenSettings, PreviousComment, PreviousFile, PreviousHunk,
    ResetFontSize, Run, SHORTCUT_GROUPS, SHORTCUTS, ToggleAllComments, ToggleCommentList,
    ToggleDiffMode, ToggleFileListMode, ToggleFileTree, ToggleFocusMode, ToggleKeyboardShortcuts,
    ToggleRunPanel, ToggleRunPanelPosition, ToggleSoftWrap, WELCOME_CONTEXT, WORKSPACE_CONTEXT,
};
use crate::markdown;
use crate::mermaid::OpenDiagram;
use crate::perf;
use crate::permission::{PermissionDialog, PermissionDialogEvent};
use crate::proposal::{ProposalEvent, ProposalSource, ProposalView};
use crate::push::{PushFlow, PushFlowEvent};
use crate::run_panel::{RunPanel, RunPanelEvent};
use crate::runtime::spawn_job;
use crate::session::{
    BranchList, Comparison, DiffTarget, RepoSession, SessionEvent, Side, open_repository,
};
use crate::settings::{
    BACKGROUND_FETCH_INTERVAL_SECS, DEFAULT_CODE_FONT_SIZE, DEFAULT_FILE_TREE_WIDTH,
    DEFAULT_RUN_PANEL_HEIGHT, DEFAULT_RUN_PANEL_WIDTH, DEFAULT_UI_FONT_SIZE, DiffViewMode,
    MIN_DOCK_SIZE, RunPanelPosition, UiSettings, clamp_font_size, clamp_tab_width,
};
use crate::text_field::{TextField, TextFieldEvent};
use crate::theme::{ActiveTheme, Theme, ThemeFonts, ThemeMode, UiStyle};
use crate::update::{UpdateStatus, Updater, UpdaterEvent};
use crate::{AppContext, OpenRequest};

/// Height of the title bar.
const TITLE_BAR_HEIGHT: Pixels = px(40.0);
/// Height of the title bar in the VS Code style.
const VSCODE_TITLE_BAR_HEIGHT: Pixels = px(35.0);
/// Tooltip of the title-bar swap button.
const SWAP_TOOLTIP: &str = "Swap base and head";
/// Height of the status bar.
const STATUS_BAR_HEIGHT: Pixels = px(28.0);
/// Height of the status bar in the VS Code style.
const VSCODE_STATUS_BAR_HEIGHT: Pixels = px(22.0);
/// Width of the VS Code style's activity bar.
const ACTIVITY_BAR_WIDTH: Pixels = px(48.0);
/// Icon size in the activity bar.
const ACTIVITY_ICON_SIZE: Pixels = px(24.0);
/// Height of the center tab bar and of the VS Code sidebar title.
const VSCODE_HEADER_HEIGHT: Pixels = px(35.0);
/// Text size of the status bar.
const STATUS_TEXT_SIZE: Pixels = px(11.5);
/// Icon size of the status-bar counters.
const STATUS_ICON_SIZE: Pixels = px(12.0);
/// Space kept for the center area when docks grow.
const MIN_CENTER_SIZE: f32 = 240.0;
/// Width of the invisible grab area of a dock resize handle.
const RESIZE_HANDLE_SIZE: Pixels = px(6.0);
/// Settings are written this long after the last change, so a dock drag
/// produces one write instead of one per mouse move.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(400);
/// How many recent repositories the empty state lists; each gets a number
/// key, `1`-`9`.
const RECENT_REPOS_SHOWN: usize = 9;
/// Size of the eye mark on the welcome screen.
const WELCOME_MARK_SIZE: f32 = 56.0;
/// Size of the "Argus" wordmark on the welcome screen.
const WELCOME_WORDMARK_SIZE: f32 = 30.0;
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

/// The tabs of the left dock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LeftTab {
    /// The changed files.
    #[default]
    Files,
    /// The review's comment threads.
    Comments,
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

/// An open proposal tab and its subscription.
struct ProposalTab {
    view: Entity<ProposalView>,
    _subscription: Subscription,
}

/// The open branch picker and its subscription.
/// What the right part of the Push split button shows.
#[derive(Clone, Copy)]
enum PushDetail {
    /// No right part.
    None,
    /// `↑ahead ↓behind` against the upstream.
    Counts(u32, u32),
    /// The head branch has no upstream yet.
    NoUpstream,
}

/// The resolved state of the Push split button.
struct PushButtonState {
    /// Whether clicking it starts a Push (or toggles the progress popover).
    enabled: bool,
    /// The left part's label: "Push" or "Pushing…".
    label: &'static str,
    /// The right part.
    detail: PushDetail,
    /// Why it is enabled or disabled.
    tooltip: String,
}

struct OpenPicker {
    picker: Entity<BranchPicker>,
    _subscription: Subscription,
}

/// The root view of the Argus window.
pub struct Workspace {
    focus_handle: FocusHandle,
    settings: UiSettings,
    settings_dirty: bool,
    /// [`UiSettings::hidden_files`], compiled; shared with the file tree
    /// and the main diff.
    hidden_files: Arc<HiddenFiles>,
    /// The settings field that adds a hidden-file pattern, and why the last
    /// pattern was refused.
    hidden_file_input: Entity<TextField>,
    hidden_file_error: Option<SharedString>,
    /// Width of the center area at the last render, which resolves
    /// [`DiffViewMode::Auto`].
    diff_area_width: f32,
    /// The docks shown (file tree, run panel) before focus mode hid them.
    focus_stash: Option<(bool, bool)>,
    /// The Claude section of the settings sheet; it owns the run defaults.
    claude_settings: Entity<ClaudeSettingsForm>,
    fonts: InstalledFonts,
    session: Option<Entity<RepoSession>>,
    session_subscriptions: Vec<Subscription>,
    /// The changed files of the session's diff (left dock).
    file_tree: Option<Entity<FileTree>>,
    /// The comment threads of the review (left dock, second tab).
    comment_list: Option<Entity<CommentList>>,
    left_tab: LeftTab,
    /// The outdated comments listed in the file tree, in its order.
    outdated_comments: Vec<argus_diff::review::CommentId>,
    diff_view: Option<Entity<DiffView>>,
    /// The runs of the open repository and their live logs (right or
    /// bottom dock).
    run_panel: Entity<RunPanel>,
    /// The main diff's events, and its repaints (the title bar shows its
    /// summary).
    diff_subscriptions: Vec<Subscription>,
    /// Proposal tabs, after the main diff tab.
    proposals: Vec<ProposalTab>,
    /// The proposal tab shown in the center; `None` shows the main diff.
    active_proposal: Option<usize>,
    /// Queued accepts and their dialogs.
    accept_flow: Entity<AcceptFlow>,
    /// The Push in flight, its progress popover and its dialogs.
    push_flow: Entity<PushFlow>,
    /// The conflict dialog, while one is open.
    conflict: Option<(Entity<ConflictDialog>, Subscription)>,
    /// The explanation dialog, while one is open.
    explanation: Option<(Entity<ExplainDialog>, Subscription)>,
    /// The Explain button's model and effort menu is open.
    explain_menu: bool,
    /// The diagram viewer, while a diagram is open in it.
    diagram_viewer: Option<(Entity<DiagramViewer>, Subscription)>,
    /// Permission requests of live runs waiting for the reviewer, oldest
    /// first.
    permissions: Vec<PermissionRequest>,
    /// The dialog of the oldest waiting request, unless hidden.
    permission_dialog: Option<(Entity<PermissionDialog>, Subscription)>,
    /// The reviewer chose Later; the status bar counter shows the dialog
    /// again.
    permissions_hidden: bool,
    _permission_task: Task<()>,
    /// Resolve runs whose proposal opens when they end.
    resolve_watches: HashMap<argus_store::RunId, Task<()>>,
    /// Fetches the head's upstream every minute, when the setting is on.
    _background_fetch: Task<()>,
    /// Release checks and the self-update.
    updater: Entity<Updater>,
    /// Focus the workspace at the next render (e.g. after leaving a tab).
    focus_requested: bool,
    /// "Viewed" marks of the last closed diff view, by (base, head), so a
    /// refresh of the same branches keeps them.
    viewed_stash: Option<(String, String, HashSet<String>)>,
    picker: Option<OpenPicker>,
    picker_closed: Option<(Side, Instant)>,
    /// Reopen the most recent repository once the list first loads.
    restore_last_repo: bool,
    /// Base and head asked for on the command line, applied to the next
    /// repository that opens.
    launch_branches: Option<(Option<String>, Option<String>)>,
    recent: RecentRepos,
    sheet: Option<Sheet>,
    notice: Option<Notice>,
    /// The detected `claude`; a blocker disables Run and shows a banner.
    claude: ClaudeStatus,
    /// The user closed the banner of the current status' warning.
    claude_warning_dismissed: bool,
    _claude_task: Task<()>,
    save_task: Option<Task<()>>,
    open_task: Option<Task<()>>,
    recent_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// Builds the shell with the settings loaded at startup, installs the
    /// theme and starts loading the recent repositories. `open` (from the
    /// command line) opens a repository instead of the most recent one.
    pub fn new(
        settings: UiSettings,
        open: Option<OpenRequest>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        let run_panel = cx.new(RunPanel::new);
        let subscriptions = vec![
            cx.observe(&run_panel, |_, _, cx| cx.notify()),
            cx.subscribe(&run_panel, |this, _, event: &RunPanelEvent, cx| {
                this.run_panel_event(event, cx)
            }),
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

        let hidden_file_input =
            cx.new(|cx| TextField::new("A file name such as .test, or a /regex/", cx));
        let accept_flow = cx.new(AcceptFlow::new);
        let mut subscriptions = subscriptions;
        subscriptions.push(cx.subscribe(
            &hidden_file_input,
            |this, input, event: &TextFieldEvent, cx| match event {
                TextFieldEvent::Submitted => this.add_hidden_file_pattern(cx),
                TextFieldEvent::Edited => {
                    if this.hidden_file_error.take().is_some() {
                        input.update(cx, |input, cx| input.set_invalid(false, cx));
                        cx.notify();
                    }
                }
            },
        ));
        subscriptions.push(cx.observe(&accept_flow, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(&accept_flow, Self::accept_flow_event));
        let push_flow = cx.new(PushFlow::new);
        subscriptions.push(cx.observe(&push_flow, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(&push_flow, Self::push_flow_event));
        let updater = cx.new(Updater::new);
        subscriptions.push(cx.observe(&updater, |_, _, cx| cx.notify()));
        subscriptions.push(cx.subscribe(&updater, |this, _, event: &UpdaterEvent, cx| {
            this.notice = Some(Notice {
                message: event.message.clone(),
                is_error: event.is_error,
            });
            cx.notify();
        }));
        let background_fetch = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(BACKGROUND_FETCH_INTERVAL_SECS))
                    .await;
                if this
                    .update(cx, |this, cx| this.background_fetch(cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut pending = cx.global::<AppContext>().runs.subscribe_permissions();
        let permission_task = cx.spawn(async move |this, cx| {
            loop {
                let requests = pending.borrow_and_update().clone();
                let applied = this.update(cx, |this, cx| this.set_permissions(requests, cx));
                if applied.is_err() || pending.changed().await.is_err() {
                    return;
                }
            }
        });

        let hidden_files = Arc::new(HiddenFiles::new(&settings.hidden_files));
        let mut this = Self {
            focus_handle,
            settings,
            settings_dirty: false,
            hidden_files,
            hidden_file_input,
            hidden_file_error: None,
            diff_area_width: 0.0,
            focus_stash: None,
            claude_settings: cx.new(ClaudeSettingsForm::new),
            fonts: InstalledFonts::load(cx),
            session: None,
            session_subscriptions: Vec::new(),
            file_tree: None,
            comment_list: None,
            left_tab: LeftTab::default(),
            outdated_comments: Vec::new(),
            diff_view: None,
            run_panel,
            diff_subscriptions: Vec::new(),
            proposals: Vec::new(),
            active_proposal: None,
            accept_flow,
            push_flow,
            updater,
            conflict: None,
            explanation: None,
            explain_menu: false,
            diagram_viewer: None,
            permissions: Vec::new(),
            permission_dialog: None,
            permissions_hidden: false,
            _permission_task: permission_task,
            resolve_watches: HashMap::new(),
            _background_fetch: background_fetch,
            focus_requested: false,
            viewed_stash: None,
            picker: None,
            picker_closed: None,
            restore_last_repo: open.is_none(),
            launch_branches: None,
            recent: RecentRepos::Loading,
            sheet: None,
            notice: None,
            claude: cx.global::<AppContext>().runs.claude_status(),
            claude_warning_dismissed: false,
            _claude_task: observe_claude_status(cx, |this: &mut Self, status, _| {
                if this.claude != status {
                    this.claude = status;
                    this.claude_warning_dismissed = false;
                }
            }),
            save_task: None,
            open_task: None,
            recent_task: None,
            _subscriptions: subscriptions,
        };
        this.apply_theme(window, cx);
        this.reload_recent(cx);
        if let Some(open) = open {
            this.launch_branches = Some((open.base, open.head));
            this.open_path(open.repo, cx);
        }
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
        cx.set_global(Theme::new(self.settings.ui_style, appearance, fonts));
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
        let theme_changed = self.settings.ui_style != before.ui_style
            || self.settings.theme != before.theme
            || self.settings.ui_font_family != before.ui_font_family
            || self.settings.code_font_family != before.code_font_family
            || self.settings.ui_font_size != before.ui_font_size
            || self.settings.code_font_size != before.code_font_size;
        if theme_changed {
            self.apply_theme(window, cx);
        }
        if self.settings.reduce_motion != before.reduce_motion {
            cx.set_reduce_motion(self.settings.reduce_motion);
        }
        self.sync_diff_options(cx);
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

    fn diff_options(&self) -> DisplayOptions {
        DisplayOptions {
            tab_width: self.settings.diff_tab_width,
            soft_wrap: self.settings.diff_soft_wrap,
            mode: if self.is_split() {
                ViewMode::Split
            } else {
                ViewMode::Unified
            },
            link_horizontal_scroll: self.settings.diff_link_horizontal_scroll,
        }
    }

    /// Whether the chrome follows VS Code rather than Zed.
    fn is_vscode(&self) -> bool {
        self.settings.ui_style == UiStyle::VsCode
    }

    fn title_bar_height(&self) -> Pixels {
        if self.is_vscode() {
            VSCODE_TITLE_BAR_HEIGHT
        } else {
            TITLE_BAR_HEIGHT
        }
    }

    fn status_bar_height(&self) -> Pixels {
        if self.is_vscode() {
            VSCODE_STATUS_BAR_HEIGHT
        } else {
            STATUS_BAR_HEIGHT
        }
    }

    /// Width of the activity bar left of the file tree; none in the Zed
    /// style.
    fn activity_bar_width(&self) -> f32 {
        if self.is_vscode() {
            ACTIVITY_BAR_WIDTH.as_f32()
        } else {
            0.0
        }
    }

    /// Whether the diff shows side by side: the setting, with `Auto`
    /// resolved against the width the diff had at the last render.
    fn is_split(&self) -> bool {
        self.settings.diff_view_mode.is_split(self.diff_area_width)
    }

    /// Width left to the center between the docks of a `viewport_width`
    /// window.
    fn center_width(&self, viewport_width: f32) -> f32 {
        let file_tree = &self.settings.file_tree;
        let run_panel = &self.settings.run_panel;
        let mut width = viewport_width - self.activity_bar_width();
        if file_tree.visible {
            width -= file_tree.width;
        }
        if run_panel.visible && run_panel.position == RunPanelPosition::Right {
            width -= run_panel.width;
        }
        width
    }

    fn sync_diff_options(&mut self, cx: &mut Context<Self>) {
        let options = self.diff_options();
        if let Some(view) = self.diff_view.clone() {
            view.update(cx, |view, cx| view.set_options(options, cx));
        }
        for tab in &self.proposals {
            tab.view
                .update(cx, |view, cx| view.set_options(options, cx));
        }
    }

    /// Adds the pattern typed in the settings, unless it is invalid or
    /// already listed.
    fn add_hidden_file_pattern(&mut self, cx: &mut Context<Self>) {
        let pattern = self.hidden_file_input.read(cx).text().trim().to_owned();
        if let Err(error) = hidden_files::validate(&pattern) {
            self.hidden_file_error = Some(error.into());
            self.hidden_file_input
                .update(cx, |input, cx| input.set_invalid(true, cx));
            cx.notify();
            return;
        }
        self.hidden_file_input.update(cx, |input, cx| {
            input.set_text("", cx);
            input.set_invalid(false, cx);
        });
        self.hidden_file_error = None;
        if self.settings.hidden_files.contains(&pattern) {
            cx.notify();
            return;
        }
        let mut patterns = self.settings.hidden_files.clone();
        patterns.push(pattern);
        self.set_hidden_file_patterns(patterns, cx);
    }

    /// Replaces the hidden-file patterns: saves them and re-filters the file
    /// tree and the main diff.
    fn set_hidden_file_patterns(&mut self, patterns: Vec<String>, cx: &mut Context<Self>) {
        if self.settings.hidden_files == patterns {
            return;
        }
        self.settings.hidden_files = patterns;
        self.hidden_files = Arc::new(HiddenFiles::new(&self.settings.hidden_files));
        let hidden_files = self.hidden_files.clone();
        if let Some(tree) = self.file_tree.clone() {
            let hidden_files = hidden_files.clone();
            tree.update(cx, |tree, cx| tree.set_hidden_files(hidden_files, cx));
        }
        if let Some(view) = self.diff_view.clone() {
            view.update(cx, |view, cx| view.set_hidden_files(hidden_files, cx));
        }
        self.schedule_save(cx);
        cx.notify();
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
        perf::repo_opening(cx);
        let app = cx.global::<AppContext>();
        let (git, store) = (app.git.clone(), app.store.clone());
        let job = spawn_job(cx, open_repository(git, store, path));
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|error| Err(error.to_string()));
            this.update(cx, |this, cx| {
                this.open_task = None;
                let branches = this.launch_branches.take();
                match result {
                    Ok((mut record, repo)) => {
                        // Command-line branches win over the remembered ones;
                        // names that do not exist fall back as usual.
                        if let Some((base, head)) = branches {
                            record.last_base = base.or(record.last_base);
                            record.last_head = head.or(record.last_head);
                        }
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
        self.close_diff(cx);
        self.proposals.clear();
        self.active_proposal = None;
        self.viewed_stash = None;
        let repo_id = record.id;
        self.run_panel
            .update(cx, |panel, cx| panel.set_repo(Some(repo_id), cx));
        let session = cx.new(|cx| RepoSession::new(record, repo, cx));
        let flat = self.settings.file_tree.flat_list;
        let hidden_files = self.hidden_files.clone();
        let file_tree = cx.new(|cx| FileTree::new(flat, hidden_files, cx));
        let comments = session.read(cx).comments().clone();
        let comment_list = cx.new(|cx| CommentList::new(comments.clone(), cx));
        self.outdated_comments.clear();
        self.session_subscriptions = vec![
            cx.observe(&session, |_, _, cx| cx.notify()),
            cx.observe(&file_tree, |_, _, cx| cx.notify()),
            cx.observe(&comment_list, |_, _, cx| cx.notify()),
            cx.subscribe(
                &comments,
                |this, comments, event: &ReviewCommentsEvent, cx| {
                    if let ReviewCommentsEvent::Changed(change) = event {
                        this.sync_comment_badges(&comments, cx);
                        match change {
                            // Comments that arrive after the diff still
                            // follow it.
                            Some(CommentChange::Loaded(_)) => this.reanchor_comments(cx),
                            // A dismissed or deleted comment finishes its runs.
                            Some(CommentChange::Updated(id) | CommentChange::Deleted(id)) => {
                                let id = *id;
                                this.run_panel
                                    .update(cx, |panel, cx| panel.comment_changed(id, cx));
                            }
                            _ => {}
                        }
                    }
                },
            ),
            cx.subscribe(
                &comment_list,
                |this, _, event: &CommentListEvent, cx| match event {
                    CommentListEvent::Open(id) => {
                        let id = *id;
                        this.activate_tab(None, cx);
                        this.update_main_diff(cx, |view, cx| view.reveal_comment(id, cx));
                    }
                    CommentListEvent::Reattach(id) => {
                        let id = *id;
                        if this.diff_view.is_some() {
                            this.activate_tab(None, cx);
                            this.update_main_diff(cx, |view, cx| view.start_reattach(id, cx));
                        } else {
                            this.notice = Some(Notice {
                                message: "Open the diff to pick the new lines.".into(),
                                is_error: false,
                            });
                            cx.notify();
                        }
                    }
                },
            ),
            cx.subscribe(
                &file_tree,
                |this, _, event: &FileTreeEvent, cx| match event {
                    FileTreeEvent::OpenFile { index, .. } => {
                        let index = *index;
                        this.activate_tab(None, cx);
                        this.update_main_diff(cx, |view, cx| view.reveal_file(index, cx));
                    }
                    FileTreeEvent::OpenOutdated(item) => {
                        // Outdated comments are not inline: the Comments tab
                        // shows the stored snippet and the Re-attach and
                        // Dismiss actions.
                        if let Some(&id) = this.outdated_comments.get(*item) {
                            if let Some(list) = this.comment_list.clone() {
                                list.update(cx, |list, cx| list.select(id, cx));
                            }
                            this.left_tab = LeftTab::Comments;
                            cx.notify();
                        }
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
                        this.follow_target(target.clone(), cx);
                    }
                },
            ),
        ];
        self.session = Some(session);
        self.file_tree = Some(file_tree);
        self.comment_list = Some(comment_list);
        self.sync_comment_badges(&comments, cx);
        cx.notify();
    }

    /// Mirrors the comments into the file tree: a state marker per file (a
    /// running run wins over a waiting proposal) and the "Outdated" group.
    fn sync_comment_badges(&mut self, comments: &Entity<ReviewComments>, cx: &mut Context<Self>) {
        let Some(file_tree) = self.file_tree.clone() else {
            return;
        };
        let snapshot = comments.read(cx).comments().clone();
        let mut markers: HashMap<SharedString, FileMarker> = HashMap::new();
        let mut outdated = Vec::new();
        let mut labels = Vec::new();
        for comment in snapshot.iter() {
            let marker = match comment.status {
                CommentStatus::Running => Some(FileMarker::Running),
                CommentStatus::Proposed => Some(FileMarker::Proposed),
                _ => None,
            };
            if let Some(marker) = marker {
                let slot = markers
                    .entry(comment.anchor.file_path.clone().into())
                    .or_insert(marker);
                *slot = (*slot).max(marker);
            }
            // Closed threads need nothing more; they do not nag.
            if comment.outdated && !comment.is_closed() {
                let first = markdown::first_line_plain(&comment.body_md);
                let anchor = &comment.anchor;
                labels.push(SharedString::from(if anchor.is_file() {
                    format!("{} · {first}", anchor.file_path)
                } else {
                    format!("{}:{} · {first}", anchor.file_path, anchor.start_line)
                }));
                outdated.push(comment.id);
            }
        }
        self.outdated_comments = outdated;
        file_tree.update(cx, |tree, cx| {
            tree.set_file_markers(markers, cx);
            tree.set_outdated(labels, cx);
        });
    }

    // ---- diff view ----------------------------------------------------------

    /// Follows the comparison: the diff view shows the file tree's diff
    /// session, so both share one file list, one +/- count pool and one
    /// hunk cache. A new target replaces (and cancels) the previous view;
    /// "Viewed" marks carry over when the branches are the same.
    fn follow_target(&mut self, target: Option<DiffTarget>, cx: &mut Context<Self>) {
        let session = self
            .file_tree
            .as_ref()
            .and_then(|tree| tree.read(cx).diff_session().cloned());
        let (Some(target), Some(session)) = (target, session) else {
            self.close_diff(cx);
            return;
        };
        if self
            .diff_view
            .as_ref()
            .is_some_and(|view| view.read(cx).target() == &target)
        {
            return;
        }
        self.close_diff(cx);
        let viewed = self
            .viewed_stash
            .take()
            .filter(|(base, head, _)| *base == target.base && *head == target.head)
            .map(|(_, _, viewed)| viewed)
            .unwrap_or_default();
        let display = self.diff_options();
        let Some(comments) = self
            .session
            .as_ref()
            .map(|session| session.read(cx).comments().clone())
        else {
            return;
        };
        let hidden_files = self.hidden_files.clone();
        let view = cx
            .new(|cx| DiffView::new(session, target, display, viewed, comments, hidden_files, cx));
        perf::watch_diff(view.downgrade(), cx);
        self.diff_subscriptions = vec![
            cx.observe(&view, |_, _, cx| cx.notify()),
            cx.subscribe(&view, |this, _, event: &DiffViewEvent, cx| match event {
                DiffViewEvent::ActiveFileChanged(index) => {
                    if let Some(tree) = this.file_tree.clone() {
                        let index = *index;
                        tree.update(cx, |tree, cx| tree.set_active_file(index, cx));
                    }
                }
                // The view opens composers itself; logged for debugging.
                DiffViewEvent::LinesSelected(selection) => {
                    tracing::debug!(?selection, "lines selected");
                }
                DiffViewEvent::CommentRequested(selection) => {
                    tracing::debug!(?selection, "comment requested");
                }
                DiffViewEvent::ExplainRequested(selection) => {
                    let selection = selection.clone();
                    this.explain_lines(selection, cx);
                }
                DiffViewEvent::SelectionRejected(hint) => {
                    tracing::debug!(%hint, "selection rejected");
                }
                DiffViewEvent::RunRequested {
                    comment_id,
                    model,
                    effort,
                } => {
                    let (comment_id, model, effort) = (*comment_id, model.clone(), effort.clone());
                    this.run_panel
                        .update(cx, |panel, cx| panel.start(comment_id, model, effort, cx));
                }
                DiffViewEvent::ViewProposal(id) => {
                    this.open_proposal(ProposalSource::Comment(*id), cx);
                }
                DiffViewEvent::AcceptProposal(run_id) => {
                    let run_id = *run_id;
                    tracing::debug!(run = %run_id, "accept requested from a thread");
                    this.accept_flow
                        .update(cx, |flow, cx| flow.accept(run_id, cx));
                }
                // Only proposal diffs offer "Open in editor".
                DiffViewEvent::OpenInEditor(_) => {}
                DiffViewEvent::ShowRunLog(run_id) => {
                    let run_id = *run_id;
                    this.show_run_panel(cx, |panel, cx| panel.show_run(run_id, cx));
                }
                DiffViewEvent::ShowCommentRun(comment_id) => {
                    let comment_id = *comment_id;
                    this.show_run_panel(cx, |panel, cx| panel.show_comment_run(comment_id, cx));
                }
            }),
        ];
        self.diff_view = Some(view);
        self.reanchor_comments(cx);
        cx.notify();
    }

    /// Re-anchors the review's comments over the diff on screen (spec §7.3):
    /// runs for every new diff (branch change, Fetch, Accept) and once the
    /// comments of the review have loaded.
    fn reanchor_comments(&mut self, cx: &mut Context<Self>) {
        let (Some(view), Some(session)) = (self.diff_view.as_ref(), self.session.as_ref()) else {
            return;
        };
        let view = view.read(cx);
        let target = view.target();
        let diff = view.session().clone();
        let comments = session.read(cx).comments().clone();
        let same_review = comments
            .read(cx)
            .key()
            .is_some_and(|key| key.base_ref == target.base && key.head_ref == target.head);
        if same_review {
            comments.update(cx, |comments, cx| comments.reanchor(diff, cx));
        }
    }

    /// Opens the run panel (hidden by default) and points it at a run.
    fn show_run_panel(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut RunPanel, &mut Context<RunPanel>),
    ) {
        if !self.settings.run_panel.visible {
            self.settings.run_panel.visible = true;
            self.schedule_save(cx);
        }
        self.run_panel.update(cx, f);
        cx.notify();
    }

    /// Drops the diff view, cancelling its jobs, and keeps its "Viewed"
    /// marks for a refresh of the same branches.
    fn close_diff(&mut self, cx: &mut Context<Self>) {
        self.diff_subscriptions.clear();
        if let Some(view) = self.diff_view.take() {
            let view = view.read(cx);
            let target = view.target();
            self.viewed_stash = Some((
                target.base.clone(),
                target.head.clone(),
                view.viewed_paths(),
            ));
            cx.notify();
        }
    }

    /// The diff view of the active tab: the main diff, or the open
    /// proposal's.
    fn active_diff(&self, cx: &gpui::App) -> Option<Entity<DiffView>> {
        match self
            .active_proposal
            .and_then(|index| self.proposals.get(index))
        {
            Some(tab) => tab.view.read(cx).diff_view(),
            None => self.diff_view.clone(),
        }
    }

    fn update_diff(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut DiffView, &mut Context<DiffView>),
    ) {
        if let Some(view) = self.active_diff(cx) {
            view.update(cx, f);
        }
    }

    /// Like [`Self::update_diff`], always on the main diff.
    fn update_main_diff(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut DiffView, &mut Context<DiffView>),
    ) {
        if let Some(view) = self.diff_view.clone() {
            view.update(cx, f);
        }
    }

    // ---- proposal tabs ------------------------------------------------------

    /// Shows the proposal of `source` in its tab, opening one if needed.
    fn open_proposal(&mut self, source: ProposalSource, cx: &mut Context<Self>) {
        if let Some(index) = self
            .proposals
            .iter()
            .position(|tab| tab.view.read(cx).shows(source))
        {
            self.activate_tab(Some(index), cx);
            return;
        }
        let display = self.diff_options();
        let view = cx.new(|cx| ProposalView::new(source, display, cx));
        let subscription = cx.subscribe(&view, Self::proposal_event);
        self.proposals.push(ProposalTab {
            view,
            _subscription: subscription,
        });
        self.activate_tab(Some(self.proposals.len() - 1), cx);
    }

    /// Shows the main diff (`None`) or a proposal tab, and moves the focus
    /// there.
    fn activate_tab(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        self.active_proposal = index.filter(|&index| index < self.proposals.len());
        match self.active_proposal {
            Some(index) => {
                let view = self.proposals[index].view.clone();
                view.update(cx, |view, cx| view.request_focus(cx));
            }
            None => self.focus_requested = true,
        }
        cx.notify();
    }

    /// Closes a proposal tab; the proposal itself stays until it is
    /// accepted or rejected.
    fn close_proposal(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.proposals.len() {
            return;
        }
        self.proposals.remove(index);
        let active = match self.active_proposal {
            Some(active) if active == index => {
                // The tab to the left, or the main diff.
                index
                    .checked_sub(1)
                    .or((!self.proposals.is_empty()).then_some(0))
            }
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        self.activate_tab(active, cx);
    }

    fn proposal_event(
        &mut self,
        view: Entity<ProposalView>,
        event: &ProposalEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ProposalEvent::RevealComment(id) => {
                let id = *id;
                self.activate_tab(None, cx);
                self.update_main_diff(cx, |view, cx| view.reveal_comment(id, cx));
                if let Some(list) = self.comment_list.clone() {
                    list.update(cx, |list, cx| list.select(id, cx));
                }
            }
            ProposalEvent::Notice { message, is_error } => {
                self.notice = Some(Notice {
                    message: message.clone(),
                    is_error: *is_error,
                });
                cx.notify();
            }
            ProposalEvent::AcceptRequested(run_id) => {
                let run_id = *run_id;
                tracing::debug!(run = %run_id, "accept requested");
                self.accept_flow
                    .update(cx, |flow, cx| flow.accept(run_id, cx));
            }
            ProposalEvent::ContinuePush { run_id, head } => {
                let (run_id, head) = (*run_id, head.clone());
                tracing::debug!(run = %run_id, "resolution accepted; continuing the push");
                if self.accept_flow.read(cx).is_busy() {
                    self.notice = Some(Notice {
                        message: "Wait for the accepts in flight, then accept the resolution."
                            .into(),
                        is_error: false,
                    });
                    cx.notify();
                    return;
                }
                self.push_flow
                    .update(cx, |flow, cx| flow.continue_push(run_id, head, cx));
            }
            ProposalEvent::Accepted(run_id) => {
                let run_id = *run_id;
                while let Some(index) = self
                    .proposals
                    .iter()
                    .position(|tab| tab.view.read(cx).run_id() == Some(run_id))
                {
                    self.close_proposal(index, cx);
                }
            }
            ProposalEvent::Replied {
                comment_id,
                followup,
            } => {
                tracing::debug!(comment = ?comment_id, run = ?followup, "proposal reply sent");
                if comment_id.is_some()
                    && let Some(session) = self.session.clone()
                {
                    let comments = session.read(cx).comments().clone();
                    comments.update(cx, |comments, cx| comments.reload(cx));
                }
                let message = if followup.is_some() {
                    "Follow-up run started: Claude resumes the same session in the proposal's \
                     worktree."
                } else {
                    "Note added to the thread."
                };
                self.notice = Some(Notice {
                    message: message.into(),
                    is_error: false,
                });
                cx.notify();
            }
            ProposalEvent::Rejected(_) => {
                if let Some(index) = self.proposals.iter().position(|tab| tab.view == view) {
                    self.close_proposal(index, cx);
                }
                self.notice = Some(Notice {
                    message: "Proposal rejected: its worktree is deleted and the comment is open \
                              again."
                        .into(),
                    is_error: false,
                });
                cx.notify();
            }
        }
    }

    /// Follows the accepts: "Accepting…" on their tabs, and a refresh of the
    /// branches (diff, comment anchors, Push counts) after each commit.
    fn accept_flow_event(
        &mut self,
        _: Entity<AcceptFlow>,
        event: &AcceptFlowEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AcceptFlowEvent::Stage { run_id, stage } => {
                for tab in &self.proposals {
                    if tab.view.read(cx).run_id() == Some(*run_id) {
                        let stage = *stage;
                        tab.view
                            .update(cx, |view, cx| view.set_accepting(stage, cx));
                    }
                }
            }
            AcceptFlowEvent::Committed(accepted) => {
                while let Some(index) = self
                    .proposals
                    .iter()
                    .position(|tab| tab.view.read(cx).run_id() == Some(accepted.run_id))
                {
                    self.close_proposal(index, cx);
                }
                if let Some(session) = self.session.clone() {
                    session.update(cx, |session, cx| session.refresh_branches(cx));
                }
                let subject = accepted.message.lines().next().unwrap_or_default();
                let mut message = format!(
                    "Committed {} on `{}`: {subject}",
                    accepted.commit.short(8),
                    accepted.head
                );
                if accepted.retried {
                    message.push_str(" (the branch had moved; applied on the new tip)");
                }
                if !accepted.warnings.is_empty() {
                    message.push_str(&format!(". Warning: {}", accepted.warnings.join("; ")));
                }
                self.notice = Some(Notice {
                    message: message.into(),
                    is_error: !accepted.warnings.is_empty(),
                });
                cx.notify();
            }
            AcceptFlowEvent::Conflict(conflict) => {
                self.open_conflict(ConflictSource::Accept(conflict.clone()), cx);
            }
            AcceptFlowEvent::DialogsClosed => self.activate_tab(self.active_proposal, cx),
            AcceptFlowEvent::Notice { message, is_error } => {
                self.notice = Some(Notice {
                    message: message.clone(),
                    is_error: *is_error,
                });
                cx.notify();
            }
        }
    }

    /// Follows the Push: refresh the branches (a rebase moved the head, the
    /// counts changed) and show its notices.
    fn push_flow_event(
        &mut self,
        _: Entity<PushFlow>,
        event: &PushFlowEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            PushFlowEvent::Finished => {
                if let Some(session) = self.session.clone() {
                    session.update(cx, |session, cx| session.reload_after_push(cx));
                }
            }
            PushFlowEvent::DialogsClosed => self.activate_tab(self.active_proposal, cx),
            PushFlowEvent::Conflict { request, conflict } => {
                self.open_conflict(
                    ConflictSource::Push {
                        request: request.clone(),
                        conflict: conflict.clone(),
                    },
                    cx,
                );
            }
            PushFlowEvent::Notice { message, is_error } => {
                self.notice = Some(Notice {
                    message: message.clone(),
                    is_error: *is_error,
                });
                cx.notify();
            }
        }
    }

    // ---- conflicts ----------------------------------------------------------

    /// Opens the conflict dialog for `source`, over any earlier one.
    fn open_conflict(&mut self, source: ConflictSource, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| ConflictDialog::new(source, cx));
        let subscription = cx.subscribe(&dialog, Self::conflict_event);
        self.conflict = Some((dialog, subscription));
        cx.notify();
    }

    fn conflict_event(
        &mut self,
        dialog: Entity<ConflictDialog>,
        event: &ConflictDialogEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConflictDialogEvent::ResolveStarted(run_id) => {
                let run_id = *run_id;
                if !self.settings.run_panel.visible {
                    self.settings.run_panel.visible = true;
                    self.schedule_save(cx);
                }
                self.watch_resolve(run_id, cx);
                self.notice = Some(Notice {
                    message: "Resolve run started: its proposal opens when it finishes (see the \
                              run panel)."
                        .into(),
                    is_error: false,
                });
            }
            ConflictDialogEvent::Closed => {
                if self
                    .conflict
                    .as_ref()
                    .is_some_and(|(open, _)| *open == dialog)
                {
                    self.conflict = None;
                    self.activate_tab(self.active_proposal, cx);
                }
                // A cancelled rebase or a resolution changes the counts.
                if let Some(session) = self.session.clone() {
                    session.update(cx, |session, cx| session.reload_after_push(cx));
                }
            }
            ConflictDialogEvent::Notice { message, is_error } => {
                self.notice = Some(Notice {
                    message: message.clone(),
                    is_error: *is_error,
                });
            }
        }
        cx.notify();
    }

    // ---- permission requests --------------------------------------------------

    /// Takes the latest list of waiting requests and shows the oldest.
    fn set_permissions(&mut self, requests: Vec<PermissionRequest>, cx: &mut Context<Self>) {
        self.permissions = requests;
        if self.permissions.is_empty() {
            self.permissions_hidden = false;
        }
        self.sync_permission_dialog(cx);
        cx.notify();
    }

    /// Opens the dialog of the oldest waiting request, keeps it when it
    /// already shows that request, or closes it when nothing waits or the
    /// reviewer chose Later.
    fn sync_permission_dialog(&mut self, cx: &mut Context<Self>) {
        let waiting = self.permissions.len();
        let Some(request) = self
            .permissions
            .first()
            .filter(|_| !self.permissions_hidden)
            .cloned()
        else {
            self.permission_dialog = None;
            return;
        };
        if let Some((dialog, _)) = &self.permission_dialog
            && dialog.read(cx).key() == (request.run_id, request.request_id.as_str())
        {
            dialog.update(cx, |dialog, cx| dialog.set_waiting(waiting, cx));
            return;
        }
        let context = self.run_panel.read(cx).run_context(request.run_id);
        let dialog = cx.new(|cx| PermissionDialog::new(request, context, waiting, cx));
        let subscription = cx.subscribe(&dialog, Self::permission_event);
        self.permission_dialog = Some((dialog, subscription));
    }

    /// Hides the permission dialog; the runs keep waiting.
    fn hide_permissions(&mut self, cx: &mut Context<Self>) {
        self.permissions_hidden = true;
        self.permission_dialog = None;
        cx.notify();
    }

    /// Shows the permission dialog again.
    fn show_permissions(&mut self, cx: &mut Context<Self>) {
        self.permissions_hidden = false;
        self.sync_permission_dialog(cx);
        cx.notify();
    }

    fn permission_event(
        &mut self,
        _: Entity<PermissionDialog>,
        event: &PermissionDialogEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            // The answered request leaves the list; the next one opens with
            // the update.
            PermissionDialogEvent::Answered => self.permission_dialog = None,
            PermissionDialogEvent::Later => self.hide_permissions(cx),
            PermissionDialogEvent::Notice { message, is_error } => {
                self.notice = Some(Notice {
                    message: message.clone(),
                    is_error: *is_error,
                });
            }
        }
        cx.notify();
    }

    /// Opens the proposal of resolve run `run_id` once it ends with
    /// something to review, or reports its failure.
    fn watch_resolve(&mut self, run_id: argus_store::RunId, cx: &mut Context<Self>) {
        let runs = cx.global::<AppContext>().runs.clone();
        let mut updates = runs.subscribe_runs();
        let store = cx.global::<AppContext>().store.clone();
        // The run may have ended before the subscription: check it once.
        let stored = crate::runtime::spawn_job(cx, async move { store.runs().get(run_id).await });
        let task = cx.spawn(async move |this, cx| {
            use argus_runs::RunUpdate;
            use tokio::sync::broadcast::error::RecvError;
            let mut ended = match stored.await {
                Ok(Ok(Some(run))) if !run.status.is_active() => Some((run.status, run.error)),
                _ => None,
            };
            while ended.is_none() {
                match updates.recv().await {
                    Ok(RunUpdate::Finished(finished)) if finished.run_id == run_id => {
                        ended = Some((finished.status, finished.error));
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                }
            }
            let Some((status, error)) = ended else {
                return;
            };
            this.update(cx, |this, cx| {
                this.resolve_watches.remove(&run_id);
                this.resolve_finished(run_id, status, error, cx);
            })
            .ok();
        });
        self.resolve_watches.insert(run_id, task);
    }

    fn resolve_finished(
        &mut self,
        run_id: argus_store::RunId,
        status: argus_store::RunStatus,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        use argus_store::RunStatus;
        match status {
            RunStatus::Succeeded | RunStatus::Stopped => {
                self.open_proposal(ProposalSource::Run(run_id), cx);
            }
            _ => {
                let reason = error
                    .as_deref()
                    .and_then(|error| error.lines().next())
                    .unwrap_or("it changed nothing")
                    .to_owned();
                self.notice = Some(Notice {
                    message: format!(
                        "The resolve run ended without a resolution ({status}: {reason}). Nothing \
                         was committed or rebased; its worktree is deleted."
                    )
                    .into(),
                    is_error: true,
                });
            }
        }
        cx.notify();
    }

    // ---- explanations -------------------------------------------------------

    /// An explanation request for the main diff, or `None` without one.
    fn explain_request(&self, scope: ExplainScope, cx: &gpui::App) -> Option<ExplainRequest> {
        let session = self.session.as_ref()?.read(cx);
        let target = self.diff_view.as_ref()?.read(cx).target();
        Some(ExplainRequest {
            repo_id: session.record().id,
            repo_path: session.record().path.clone(),
            base: target.base.clone(),
            head: target.head.clone(),
            merge_base: target.merge_base.to_hex(),
            head_tip: target.head_tip.to_hex(),
            scope,
        })
    }

    /// Opens the explanation of the whole main diff.
    fn explain_diff(&mut self, cx: &mut Context<Self>) {
        let hidden = self
            .diff_view
            .as_ref()
            .map(|view| view.read(cx).hidden_paths())
            .unwrap_or_default();
        let Some(request) = self.explain_request(ExplainScope::Diff { hidden }, cx) else {
            return;
        };
        let title = format!("{}...{}", request.base, request.head);
        self.open_explanation(request, title.into(), cx);
    }

    /// Opens the explanation of lines selected in the main diff.
    fn explain_lines(&mut self, selection: LineSelection, cx: &mut Context<Self>) {
        let lines = if selection.end > selection.start {
            format!("lines {}–{}", selection.start, selection.end)
        } else {
            format!("line {}", selection.start)
        };
        let title = format!("{} · {lines}", selection.file);
        let scope = ExplainScope::Lines {
            file: selection.file,
            old_file: selection.old_file,
            side: selection.side,
            start: selection.start,
            end: selection.end,
        };
        let Some(request) = self.explain_request(scope, cx) else {
            return;
        };
        self.open_explanation(request, title.into(), cx);
    }

    fn open_explanation(
        &mut self,
        request: ExplainRequest,
        title: SharedString,
        cx: &mut Context<Self>,
    ) {
        if let Some(blocker) = self.claude.run_blocker() {
            self.notice = Some(Notice {
                message: blocker.into(),
                is_error: true,
            });
            cx.notify();
            return;
        }
        self.explain_menu = false;
        if self
            .explanation
            .as_ref()
            .is_some_and(|(dialog, _)| dialog.read(cx).explains(&request))
        {
            return;
        }
        let dialog = cx.new(|cx| ExplainDialog::new(request, title, cx));
        let subscription =
            cx.subscribe(
                &dialog,
                |this, _, event: &ExplainDialogEvent, cx| match event {
                    ExplainDialogEvent::Closed => this.close_explanation(cx),
                },
            );
        self.explanation = Some((dialog, subscription));
        cx.notify();
    }

    /// Clears the gutter selection of the main diff while it is shown.
    /// Returns whether there was one.
    fn clear_main_selection(&mut self, cx: &mut Context<Self>) -> bool {
        self.active_proposal.is_none()
            && self
                .diff_view
                .clone()
                .is_some_and(|view| view.update(cx, |view, cx| view.clear_selection(cx)))
    }

    fn close_explanation(&mut self, cx: &mut Context<Self>) {
        if self.explanation.take().is_some() {
            self.focus_requested = true;
            cx.notify();
        }
    }

    fn open_diagram(&mut self, action: &OpenDiagram, window: &mut Window, cx: &mut Context<Self>) {
        let viewer = cx.new(|cx| DiagramViewer::new(action.diagram.clone(), cx));
        window.focus(&viewer.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(
            &viewer,
            window,
            |this, _, event: &DiagramViewerEvent, window, cx| match event {
                DiagramViewerEvent::Closed => this.close_diagram(window, cx),
            },
        );
        if let Some((old, _)) = self.diagram_viewer.replace((viewer, subscription)) {
            old.update(cx, |old, cx| old.release(window, cx));
        }
        cx.notify();
    }

    fn close_diagram(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((viewer, _)) = self.diagram_viewer.take() {
            viewer.update(cx, |viewer, cx| viewer.release(window, cx));
            self.focus_requested = true;
            cx.notify();
        }
    }

    /// `e`: explains the lines selected in the main diff, or the whole diff.
    fn explain(&mut self, _: &Explain, _: &mut Window, cx: &mut Context<Self>) {
        if self.diff_view.is_none() {
            self.notice = Some(Notice {
                message: "Open a diff to explain it.".into(),
                is_error: false,
            });
            cx.notify();
            return;
        }
        let selected = self.active_proposal.is_none()
            && self
                .diff_view
                .clone()
                .is_some_and(|view| view.update(cx, |view, cx| view.explain_selection(cx)));
        if !selected {
            self.explain_diff(cx);
        }
    }

    /// The Explain split button: "Explain" opens the explanation of the
    /// main diff, the right part (its model and effort) their menu.
    fn render_explain_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.diff_view.as_ref()?;
        let theme = cx.theme();
        let colors = theme.colors.clone();
        let blocker = self.claude.run_blocker();
        let enabled = blocker.is_none();
        let (label_color, detail_color) = if enabled {
            (colors.text, colors.text_muted)
        } else {
            (colors.text_placeholder, colors.text_placeholder)
        };
        let (model, effort) = explain::choice(cx);
        let main = div()
            .id("title-explain")
            .flex()
            .items_center()
            .gap_1p5()
            .h_full()
            .px(px(9.0))
            .text_color(label_color)
            .child(icon(IconName::Sparkle, px(14.0), label_color))
            .child("Explain")
            .tooltip(text_tooltip(blocker.clone().unwrap_or_else(|| {
                format!(
                    "Claude explains this diff: before and after, step by step ({})",
                    shortcut_text::<Explain>()
                )
            })))
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .active(|style| style.bg(colors.element_selected))
                    .on_click(cx.listener(|this, _, _, cx| this.explain_diff(cx)))
            });
        let open = self.explain_menu;
        let detail = div()
            .id("title-explain-choice")
            .flex()
            .items_center()
            .gap_1()
            .h_full()
            .px_2()
            .border_l_1()
            .border_color(colors.border)
            .text_xs()
            .text_color(detail_color)
            .cursor_pointer()
            .when(open, |this| this.bg(colors.element_selected))
            .hover(|style| style.bg(colors.element_hover))
            .on_click(cx.listener(|this, _, _, cx| {
                this.explain_menu = !this.explain_menu;
                cx.notify();
            }))
            .child(followup::label(&model, &effort))
            .child(icon(IconName::CaretDown, px(10.0), detail_color));
        let menu = open.then(|| {
            deferred(
                div()
                    .absolute()
                    .top(px(32.0))
                    .right_0()
                    .child(explain::choice_menu(
                        "title-explain-menu",
                        |this: &mut Self, cx| {
                            this.explain_menu = false;
                            cx.notify();
                        },
                        cx,
                    )),
            )
            .with_priority(2)
        });
        Some(
            div()
                .relative()
                .flex_none()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(28.0))
                        .rounded(radius::md(cx))
                        .border_1()
                        .border_color(colors.border)
                        .overflow_hidden()
                        .text_sm()
                        .child(main)
                        .child(detail),
                )
                .children(menu)
                .into_any_element(),
        )
    }

    /// The title bar's Push button was clicked: push to the upstream, or
    /// offer `git push -u` when the head has none.
    fn start_push(&mut self, cx: &mut Context<Self>) {
        self.close_picker(cx);
        if self.push_flow.read(cx).is_running() {
            self.push_flow
                .update(cx, |flow, cx| flow.toggle_popover(cx));
            return;
        }
        if self.accept_flow.read(cx).is_busy() {
            return;
        }
        let Some(session) = self.session.as_ref().map(|session| session.read(cx)) else {
            return;
        };
        let Some(head) = session.local_head().map(|branch| branch.name.clone()) else {
            return;
        };
        let request = argus_runs::PushRequest {
            repo_id: session.record().id,
            repo: session.repo().clone(),
            head,
            mode: argus_runs::PushMode::Upstream,
        };
        if session.head_has_upstream() {
            self.push_flow.update(cx, |flow, cx| flow.push(request, cx));
        } else {
            let remotes = session.remotes().to_vec();
            self.push_flow
                .update(cx, |flow, cx| flow.offer_set_upstream(request, remotes, cx));
        }
    }

    /// The minute tick: fetch the head's upstream, unless the setting is
    /// off or a Push is running (it fetches itself).
    fn background_fetch(&mut self, cx: &mut Context<Self>) {
        if !self.settings.background_fetch || self.push_flow.read(cx).is_running() {
            return;
        }
        if let Some(session) = self.session.clone() {
            session.update(cx, |session, cx| session.fetch_head_upstream(cx));
        }
    }

    /// `a` outside a proposal tab: there is nothing to accept here.
    fn accept(&mut self, _: &Accept, _: &mut Window, cx: &mut Context<Self>) {
        self.notice = Some(Notice {
            message: "Open a proposal (\"View proposal\" on a Proposed comment) to accept it."
                .into(),
            is_error: false,
        });
        cx.notify();
    }

    fn comment(&mut self, _: &Comment, _: &mut Window, cx: &mut Context<Self>) {
        self.update_diff(cx, |view, cx| view.comment_on_current_line(cx));
    }

    fn next_comment(&mut self, _: &NextComment, _: &mut Window, cx: &mut Context<Self>) {
        self.update_diff(cx, |view, cx| view.next_comment(cx));
    }

    fn previous_comment(&mut self, _: &PreviousComment, _: &mut Window, cx: &mut Context<Self>) {
        self.update_diff(cx, |view, cx| view.previous_comment(cx));
    }

    fn toggle_all_comments(
        &mut self,
        _: &ToggleAllComments,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_diff(cx, |view, cx| view.toggle_all_threads(cx));
    }

    fn advance_comment_status(
        &mut self,
        _: &AdvanceCommentStatus,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_diff(cx, |view, cx| view.advance_focused_status(cx));
    }

    /// Switches the left dock between files and comments, showing it.
    fn toggle_comment_list(
        &mut self,
        _: &ToggleCommentList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = if self.left_tab == LeftTab::Comments && self.settings.file_tree.visible {
            LeftTab::Files
        } else {
            LeftTab::Comments
        };
        self.show_left_tab(tab, window, cx);
    }

    fn show_left_tab(&mut self, tab: LeftTab, window: &mut Window, cx: &mut Context<Self>) {
        self.left_tab = tab;
        if !self.settings.file_tree.visible {
            self.update_settings(window, cx, |settings| settings.file_tree.visible = true);
        }
        cx.notify();
    }

    fn next_hunk(&mut self, _: &NextHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.update_diff(cx, |view, cx| view.next_hunk(cx));
    }

    fn previous_hunk(&mut self, _: &PreviousHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.update_diff(cx, |view, cx| view.previous_hunk(cx));
    }

    fn toggle_soft_wrap(
        &mut self,
        _: &ToggleSoftWrap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_settings(window, cx, |settings| {
            settings.diff_soft_wrap = !settings.diff_soft_wrap;
        });
    }

    /// Switches the diff between unified and side-by-side; the choice is
    /// saved as the mode later diffs open in (no longer `Auto`).
    fn toggle_diff_mode(
        &mut self,
        _: &ToggleDiffMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = if self.is_split() {
            DiffViewMode::Unified
        } else {
            DiffViewMode::SideBySide
        };
        self.update_settings(window, cx, |settings| settings.diff_view_mode = mode);
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

    /// Whether the welcome screen is what the window shows: no repository,
    /// no proposal tab and no sheet over it.
    fn is_welcome_shown(&self) -> bool {
        self.session.is_none() && self.active_proposal.is_none() && self.sheet.is_none()
    }

    /// Opens the recent repository at `index` (the `1`-`9` keys of the
    /// welcome screen). Does nothing once a repository is open or opening.
    fn open_recent(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.is_welcome_shown() || self.open_task.is_some() {
            return;
        }
        let RecentRepos::Loaded(repos) = &self.recent else {
            return;
        };
        if let Some(repo) = repos.iter().take(RECENT_REPOS_SHOWN).nth(index) {
            let path = repo.path.clone();
            self.open_path(path, cx);
        }
    }

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
        if self.permission_dialog.is_some() {
            self.hide_permissions(cx);
            return;
        }
        if self.diagram_viewer.is_some() {
            self.close_diagram(window, cx);
            return;
        }
        if self.explain_menu {
            self.explain_menu = false;
            cx.notify();
            return;
        }
        if self.explanation.is_some() {
            self.close_explanation(cx);
            return;
        }
        if let Some(dialog) = self.conflict.as_ref().map(|(dialog, _)| dialog.clone()) {
            dialog.update(cx, |dialog, cx| dialog.cancel(cx));
            return;
        }
        if self.push_flow.read(cx).is_visible() {
            self.push_flow.update(cx, |flow, cx| flow.dismiss(cx));
            return;
        }
        if self.accept_flow.read(cx).has_dialog() {
            self.accept_flow.update(cx, |flow, cx| flow.dismiss(cx));
        } else if self.run_panel.read(cx).has_dialog() {
            self.run_panel
                .update(cx, |panel, cx| panel.close_dialog(cx));
        } else if self.picker.is_some() {
            self.close_picker(cx);
            window.focus(&self.focus_handle, cx);
        } else if tracking {
            self.update_session(cx, |session, cx| session.cancel_tracking(cx));
        } else if self.sheet.take().is_some() {
            cx.notify();
        } else if self
            .diff_view
            .as_ref()
            .is_some_and(|view| view.read(cx).is_reattaching())
        {
            self.update_main_diff(cx, |view, cx| view.cancel_reattach(cx));
        } else if !self.clear_main_selection(cx) {
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

    /// `j` / `k`: the next or previous file of the diff, in the diff's
    /// (path) order like `n` / `p`; the tree follows through
    /// [`DiffViewEvent::ActiveFileChanged`]. Without a diff view, the tree's
    /// own display order.
    fn step_file(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(view) = self.active_diff(cx) {
            view.update(cx, |view, cx| {
                if delta > 0 {
                    view.next_file(cx);
                } else {
                    view.previous_file(cx);
                }
            });
        } else if let Some(file_tree) = self.file_tree.clone() {
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
        self.show_left_tab(LeftTab::Files, window, cx);
        file_tree.update(cx, |tree, cx| tree.request_filter(cx));
        let handle = file_tree.read(cx).filter_focus_handle().clone();
        window.focus(&handle, cx);
    }

    /// Focus mode: hides both docks so the diff takes the window, and brings
    /// back the ones that were shown on the next press.
    fn toggle_focus_mode(
        &mut self,
        _: &ToggleFocusMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = (
            self.settings.file_tree.visible,
            self.settings.run_panel.visible,
        );
        let (file_tree, run_panel) = if shown.0 || shown.1 {
            self.focus_stash = Some(shown);
            (false, false)
        } else {
            self.focus_stash.take().unwrap_or((true, false))
        };
        self.update_settings(window, cx, |settings| {
            settings.file_tree.visible = file_tree;
            settings.run_panel.visible = run_panel;
        });
    }

    fn toggle_file_list_mode(
        &mut self,
        _: &ToggleFileListMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(file_tree) = self.file_tree.clone() {
            file_tree.update(cx, |tree, cx| tree.toggle_mode(cx));
        } else {
            // No tree to report the change: the next one opens in this mode.
            self.settings.file_tree.flat_list = !self.settings.file_tree.flat_list;
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// `r`: runs the focused comment thread with its Run menu choice. Run
    /// is disabled while `claude` is missing, still being checked, or lacks
    /// a required flag (spec Q4); the reason goes to the status bar.
    fn run(&mut self, _: &Run, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(blocker) = self.claude.run_blocker() {
            self.notice = Some(Notice {
                message: blocker.into(),
                is_error: true,
            });
            cx.notify();
            return;
        }
        // In a proposal tab `r` replies to the proposal; it never gets here.
        if self.diff_view.is_none() || self.active_proposal.is_some() {
            self.notice = Some(Notice {
                message: "Open the diff and click a comment thread to run it.".into(),
                is_error: false,
            });
            cx.notify();
            return;
        }
        self.update_main_diff(cx, |view, cx| view.run_focused_comment(cx));
    }

    /// Reacts to the run panel: shows it when a run starts, jumps to a
    /// run's comment, relays notices to the status bar.
    fn run_panel_event(&mut self, event: &RunPanelEvent, cx: &mut Context<Self>) {
        match event {
            RunPanelEvent::Started(run_id) => {
                tracing::debug!(run = %run_id, "run started from the UI");
                if !self.settings.run_panel.visible {
                    self.settings.run_panel.visible = true;
                    self.schedule_save(cx);
                }
            }
            RunPanelEvent::OpenComment(id) => {
                let id = *id;
                self.activate_tab(None, cx);
                self.update_main_diff(cx, |view, cx| view.reveal_comment(id, cx));
                if let Some(list) = self.comment_list.clone() {
                    list.update(cx, |list, cx| list.select(id, cx));
                }
            }
            RunPanelEvent::ViewProposal(run_id) => {
                self.open_proposal(ProposalSource::Run(*run_id), cx);
            }
            RunPanelEvent::Notice { message, is_error } => {
                self.notice = Some(Notice {
                    message: message.clone(),
                    is_error: *is_error,
                });
            }
        }
        cx.notify();
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
        let activity_bar_width = self.activity_bar_width();
        let left_dock_width = activity_bar_width
            + if self.settings.file_tree.visible {
                self.settings.file_tree.width
            } else {
                0.0
            };
        let (title_bar_height, status_bar_height) =
            (self.title_bar_height(), self.status_bar_height());
        let x = position.x.as_f32();
        let y = position.y.as_f32();
        self.update_settings(window, cx, |settings| match dock {
            DockResize::FileTree => {
                let max = width - activity_bar_width - right_dock_width - MIN_CENTER_SIZE;
                settings.file_tree.width = clamp_size(x - activity_bar_width, max);
            }
            DockResize::RunPanel => match settings.run_panel.position {
                RunPanelPosition::Right => {
                    let max = width - left_dock_width - MIN_CENTER_SIZE;
                    settings.run_panel.width = clamp_size(width - x, max);
                }
                RunPanelPosition::Bottom => {
                    let bottom = height - status_bar_height.as_f32();
                    let max = bottom - title_bar_height.as_f32() - MIN_CENTER_SIZE;
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

    /// The title bar (Panoptes #56 §5): the breadcrumb group (eye mark,
    /// repo, base, swap, head) on the left; the diff mode segmented control,
    /// Fetch and the Push split button on the right.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let session = self.session.as_ref().map(|session| session.read(cx));
        let repo_label: SharedString = session
            .map(|session| SharedString::from(session.record().name.clone()))
            .unwrap_or_else(|| "Open Repository…".into());

        let branch_controls = session.map(|session| {
            let loaded = matches!(session.branches(), BranchList::Loaded(_));
            let swap = if loaded {
                icon_button("title-swap", IconName::ArrowsLeftRight, SWAP_TOOLTIP, cx).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.close_picker(cx);
                        this.update_session(cx, |session, cx| session.swap(cx));
                    }),
                )
            } else {
                disabled_icon_button("title-swap", IconName::ArrowsLeftRight, SWAP_TOOLTIP, cx)
            };
            div()
                .flex()
                .items_center()
                .gap_0p5()
                .min_w_0()
                .child(
                    div()
                        .flex_none()
                        .px_0p5()
                        .text_color(colors.text_placeholder)
                        .child("/"),
                )
                .child(self.render_branch_button(Side::Base, session, loaded, cx))
                .child(swap.w(px(22.0)))
                .child(self.render_branch_button(Side::Head, session, loaded, cx))
        });

        let breadcrumb = div()
            .flex()
            .items_center()
            .gap_0p5()
            .min_w_0()
            .h(px(28.0))
            .pl_2()
            .pr(px(3.0))
            .rounded(radius::md(cx))
            .bg(colors.surface)
            .border_1()
            .border_color(colors.border_variant)
            .child(eye_mark(px(16.0), cx))
            .child(
                button("title-repo", repo_label, cx)
                    .h(px(22.0))
                    .px_1p5()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(OpenRepository.boxed_clone(), cx)
                    }),
            )
            .children(branch_controls);

        // Without a remote there is nothing to fetch: no button at all.
        let fetch = match session {
            Some(session) if session.is_fetching() => Some(disabled_icon_button(
                "title-fetch",
                IconName::ArrowLineDown,
                "Fetching…",
                cx,
            )),
            Some(session) if session.has_remotes() => Some(
                icon_button("title-fetch", IconName::ArrowLineDown, "Fetch", cx).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.update_session(cx, |session, cx| session.fetch(cx));
                    }),
                ),
            ),
            _ => None,
        };
        let push = self.render_push_button(session, cx);
        let update = self.render_update_button(cx);
        let explain = self.render_explain_button(cx);
        let actions = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1p5()
            .children(update)
            .children(explain)
            .children(fetch)
            .children(push);

        let bar = div()
            .id("title-bar")
            .window_control_area(WindowControlArea::Drag)
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .h(self.title_bar_height())
            .pl(TRAFFIC_LIGHTS_WIDTH)
            .pr(px(10.0))
            .gap_2()
            .bg(colors.title_bar)
            .border_b_1()
            .border_color(colors.border_variant)
            .on_click(|event, window, _| {
                if event.click_count() == 2 && cfg!(target_os = "macos") {
                    window.titlebar_double_click();
                }
            });
        if self.is_vscode() {
            // VS Code's command center: the breadcrumb centered between two
            // equal sides.
            bar.child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .min_w_0()
                    .children(self.render_diff_summary(cx)),
            )
            .child(breadcrumb)
            .child(div().flex().flex_1().justify_end().min_w_0().child(actions))
            .into_any_element()
        } else {
            bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .child(breadcrumb)
                    .children(self.render_diff_summary(cx)),
            )
            .child(actions)
            .into_any_element()
        }
    }

    /// `4 files +27 −5 · 2 hidden` after the breadcrumb, for the main diff;
    /// "2 hidden" opens the settings, where the patterns are.
    fn render_diff_summary(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let summary = self.diff_view.as_ref()?.read(cx).summary()?;
        let theme = cx.theme();
        let colors = &theme.colors;
        let mono = |text: String, color| {
            div()
                .flex_none()
                .font_family(theme.fonts.code_family.clone())
                .text_size(px(12.0))
                .text_color(color)
                .child(text)
        };
        let files = summary.files;
        let hidden = (summary.hidden > 0).then(|| {
            div()
                .id("title-hidden-files")
                .flex_none()
                .px_1()
                .rounded(radius::sm(cx))
                .cursor_pointer()
                .text_color(colors.text_placeholder)
                .hover(|style| style.bg(colors.element_hover))
                .tooltip(text_tooltip("Files hidden by your hidden-file patterns"))
                .on_click(|_, window, cx| window.dispatch_action(OpenSettings.boxed_clone(), cx))
                .child(format!("· {} hidden", summary.hidden))
        });
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1p5()
                .text_sm()
                .child(
                    div()
                        .text_color(colors.text_muted)
                        .child(format!("{files} file{}", if files == 1 { "" } else { "s" })),
                )
                .child(mono(format!("+{}", summary.added), colors.diff_added_text))
                .child(mono(
                    format!("−{}", summary.removed),
                    colors.diff_removed_text,
                ))
                .children(hidden)
                .into_any_element(),
        )
    }

    /// The Push split button and its state (spec §5.4): `Push | ↑ahead
    /// ↓behind`, disabled with a tooltip when there is nothing to do or an
    /// Accept is in flight, `Push | No upstream` to offer `git push -u`,
    /// "Pushing…" (which toggles the progress popover) while a Push runs.
    /// "Update" while a newer release is available; disabled, with the reason
    /// in its tooltip, when this copy of the app cannot replace itself.
    fn render_update_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let updater = self.updater.read(cx);
        let element = match updater.status() {
            UpdateStatus::Idle => return None,
            UpdateStatus::Installing(release) => disabled_ghost_button_with_icon(
                "title-update",
                IconName::ArrowLineDown,
                "Updating…",
                cx,
            )
            .tooltip(text_tooltip(format!(
                "Installing Argus {}; Argus restarts when it is done.",
                release.version
            )))
            .into_any_element(),
            UpdateStatus::Available(release) => match updater.blocker() {
                Some(blocker) => disabled_ghost_button_with_icon(
                    "title-update",
                    IconName::ArrowLineDown,
                    "Update",
                    cx,
                )
                .tooltip(text_tooltip(format!(
                    "Argus {} is available. {blocker}",
                    release.version
                )))
                .into_any_element(),
                None => {
                    accent_button_with_icon("title-update", IconName::ArrowLineDown, "Update", cx)
                        .tooltip(text_tooltip(format!(
                            "Install Argus {} and restart",
                            release.version
                        )))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.updater
                                .update(cx, |updater, cx| updater.install(window, cx));
                        }))
                        .into_any_element()
                }
            },
        };
        Some(element)
    }

    fn render_push_button(
        &self,
        session: Option<&RepoSession>,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let state = |enabled: bool, label: &'static str, detail: PushDetail, tooltip: String| {
            PushButtonState {
                enabled,
                label,
                detail,
                tooltip,
            }
        };
        let state = 'state: {
            if self.push_flow.read(cx).is_running() {
                let detail = session
                    .and_then(RepoSession::upstream)
                    .map_or(PushDetail::None, |status| {
                        PushDetail::Counts(status.ahead, status.behind)
                    });
                break 'state state(
                    true,
                    "Pushing…",
                    detail,
                    "Show the push progress".to_owned(),
                );
            }
            let Some(session) = session else {
                break 'state state(
                    false,
                    "Push",
                    PushDetail::None,
                    "Open a repository first.".to_owned(),
                );
            };
            let Some(head) = session.local_head() else {
                break 'state state(
                    false,
                    "Push",
                    PushDetail::None,
                    "Pick a local head branch to push.".to_owned(),
                );
            };
            let counts = session.upstream().map_or(PushDetail::None, |status| {
                PushDetail::Counts(status.ahead, status.behind)
            });
            if self.accept_flow.read(cx).is_busy() {
                break 'state state(
                    false,
                    "Push",
                    counts,
                    "An Accept is in progress; Push is available once it lands.".to_owned(),
                );
            }
            if !session.head_has_upstream() {
                if !session.has_remotes() {
                    break 'state state(
                        false,
                        "Push",
                        PushDetail::NoUpstream,
                        "This repository has no remote to push to.".to_owned(),
                    );
                }
                break 'state state(
                    true,
                    "Push",
                    PushDetail::NoUpstream,
                    format!(
                        "`{}` has no upstream: push it with `git push -u`.",
                        head.name
                    ),
                );
            }
            match session.upstream() {
                Some(status) if status.ahead == 0 && status.behind == 0 => state(
                    false,
                    "Push",
                    counts,
                    format!(
                        "Nothing to push: `{}` is up to date with `{}`.",
                        head.name, status.upstream
                    ),
                ),
                Some(status) if status.ahead == 0 => state(
                    true,
                    "Push",
                    counts,
                    format!(
                        "`{}` is behind `{}`: pull it (rebase) and push what is left.",
                        head.name, status.upstream
                    ),
                ),
                Some(status) => state(
                    true,
                    "Push",
                    counts,
                    format!(
                        "Push `{}` to `{}`; pulls with rebase first when the remote is ahead. \
                         Never forced.",
                        head.name, status.upstream
                    ),
                ),
                None => state(
                    true,
                    "Push",
                    PushDetail::None,
                    format!("Push `{}` to its upstream.", head.name),
                ),
            }
        };
        // Push shows up only when there is something to do; the disabled
        // states would be chrome to read past on every review.
        state
            .enabled
            .then(|| self.render_push_split_button(state, cx))
    }

    /// Draws a [`PushButtonState`]: the `arrow-line-up` + label part and,
    /// when there is a detail, a second part after a divider.
    fn render_push_split_button(&self, state: PushButtonState, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let colors = &theme.colors;
        let (label_color, detail_color, ahead_color) = if state.enabled {
            (colors.text, colors.text_muted, colors.accent)
        } else {
            (
                colors.text_placeholder,
                colors.text_placeholder,
                colors.text_placeholder,
            )
        };
        let main = div()
            .flex()
            .items_center()
            .gap_1p5()
            .h_full()
            .px(px(9.0))
            .text_color(label_color)
            .child(icon(IconName::ArrowLineUp, px(14.0), label_color))
            .child(state.label);
        let detail_part = |content: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .h_full()
                .px_2()
                .border_l_1()
                .border_color(colors.border)
                .text_color(detail_color)
                .child(content)
        };
        let detail = match state.detail {
            PushDetail::None => None,
            // The tooltip says it publishes the branch.
            PushDetail::NoUpstream => None,
            PushDetail::Counts(ahead, behind) => Some(detail_part(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .font_family(theme.fonts.code_family.clone())
                    .text_size(px(11.5))
                    .child(
                        div()
                            .when(ahead > 0, |this| this.text_color(ahead_color))
                            .child(format!("↑{ahead}")),
                    )
                    .child(div().child(format!("↓{behind}")))
                    .into_any_element(),
            )),
        };
        div()
            .id("title-push")
            .flex()
            .flex_none()
            .items_center()
            .h(px(28.0))
            .rounded(radius::md(cx))
            .border_1()
            .border_color(colors.border)
            .overflow_hidden()
            .text_sm()
            .child(main)
            .children(detail)
            .tooltip(text_tooltip(state.tooltip))
            .when(state.enabled, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .active(|style| style.bg(colors.element_selected))
                    .on_click(cx.listener(|this, _, _, cx| this.start_push(cx)))
            })
            .into_any_element()
    }

    /// A base / head branch button (`git-branch` icon + name in mono), with
    /// its picker popover when open. The head branch is drawn in `accent`
    /// on `accent_soft`, the base branch in `text_muted`.
    fn render_branch_button(
        &self,
        side: Side,
        session: &RepoSession,
        enabled: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let colors = &theme.colors;
        let id = match side {
            Side::Base => "title-base",
            Side::Head => "title-head",
        };
        let (label, placeholder): (SharedString, bool) = match session.selected(side) {
            Some(name) => (name.to_string().into(), false),
            None => ("pick".into(), true),
        };
        let picker = self
            .picker
            .as_ref()
            .filter(|open| open.picker.read(cx).side() == side)
            .map(|open| open.picker.clone());
        let color = if !enabled || placeholder {
            colors.text_placeholder
        } else {
            match side {
                Side::Base => colors.text_muted,
                Side::Head => colors.accent,
            }
        };
        let fill = match side {
            Side::Head if enabled && !placeholder => Some(colors.accent_soft),
            _ => None,
        };
        let trigger = div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.0))
            .h(px(22.0))
            .max_w(px(260.0))
            .px_1p5()
            .rounded(radius::sm(cx))
            .overflow_hidden()
            .whitespace_nowrap()
            .font_family(theme.fonts.code_family.clone())
            .text_size(px(12.0))
            .text_color(color)
            .when_some(fill, |this, fill| this.bg(fill))
            .child(icon(IconName::GitBranch, px(13.0), color))
            .child(div().min_w_0().overflow_hidden().child(label))
            .tooltip(text_tooltip(match side {
                Side::Base => "Base branch",
                Side::Head => "Head branch",
            }))
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .active(|style| style.bg(colors.element_selected))
                    .when(picker.is_some(), |this| this.bg(colors.element_selected))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_picker(side, window, cx);
                    }))
            });
        div()
            .relative()
            .min_w_0()
            .child(trigger)
            .when_some(picker, |slot, picker| {
                slot.child(
                    deferred(div().absolute().top(px(28.0)).left_0().child(picker))
                        .with_priority(1),
                )
            })
    }

    /// The banner under the title bar when runs are disabled (`claude`
    /// missing, unusable or lacking a required flag) or degraded (no
    /// `--effort`). Nothing while detection is in progress.
    fn render_claude_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (message, is_error) = match &self.claude {
            ClaudeStatus::Detecting => return None,
            status => match status.run_blocker() {
                Some(blocker) => (format!("{blocker}. Run is disabled."), true),
                None if self.claude_warning_dismissed => return None,
                None => (status.warning()?, false),
            },
        };
        let colors = cx.theme().colors.clone();
        let tint = if is_error {
            colors.error
        } else {
            colors.warning
        };
        let actions = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .child(
                button("claude-banner-recheck", "Check again", cx)
                    .h(px(20.0))
                    .text_xs()
                    .on_click(|_, _, cx| cx.global::<AppContext>().runs.recheck_claude()),
            )
            .child(
                button("claude-banner-settings", "Settings", cx)
                    .h(px(20.0))
                    .text_xs()
                    .on_click(|_, window, cx| {
                        window.dispatch_action(OpenSettings.boxed_clone(), cx)
                    }),
            )
            .when(!is_error, |actions| {
                actions.child(
                    button("claude-banner-dismiss", "Dismiss", cx)
                        .h(px(20.0))
                        .text_xs()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.claude_warning_dismissed = true;
                            cx.notify();
                        })),
                )
            });
        Some(
            div()
                .id("claude-banner")
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap_2()
                .px_2()
                .py_0p5()
                .bg(tint.opacity(0.12))
                .border_b_1()
                .border_color(tint.opacity(0.4))
                .text_xs()
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(tint)
                        .child(message),
                )
                .child(actions)
                .into_any_element(),
        )
    }

    fn render_file_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.render_left_tabs(cx);
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
            (Some(Comparison::Ready(_)), Some(_)) if self.left_tab == LeftTab::Comments => {
                match self.comment_list.clone() {
                    Some(list) => list.into_any_element(),
                    None => placeholder("No comments.", cx).into_any_element(),
                }
            }
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
                    .when(!self.is_vscode(), |body| {
                        body.child(panel_header("Files", cx))
                    })
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
            .child(tabs)
            .child(body)
            .child(self.resize_handle(DockResize::FileTree, false, cx))
    }

    /// "Files" | "Comments" above the left dock (Panoptes #56 §5): SM
    /// tabs, the selected one on `element_selected`. The status bar counts
    /// the comments.
    fn render_left_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        if self.is_vscode() {
            // The activity bar switches the view; the sidebar only names it.
            let title = match self.left_tab {
                LeftTab::Files => "FILES",
                LeftTab::Comments => "COMMENTS",
            };
            return div()
                .flex()
                .flex_none()
                .items_center()
                .h(VSCODE_HEADER_HEIGHT)
                .px(px(20.0))
                .text_size(px(11.0))
                .text_color(colors.text)
                .child(title)
                .into_any_element();
        }
        let tab = |id: &'static str, label: &'static str, tab: LeftTab, cx: &mut Context<Self>| {
            let active = self.left_tab == tab;
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.0))
                .h(px(24.0))
                .px(px(10.0))
                .rounded(radius::sm(cx))
                .text_size(px(12.0))
                .cursor_pointer()
                .text_color(if active {
                    colors.text
                } else {
                    colors.text_muted
                })
                .when(active, |tab| tab.bg(colors.element_selected))
                .when(!active, |tab| {
                    tab.hover(|style| style.bg(colors.element_hover))
                })
                .on_click(
                    cx.listener(move |this, _, window, cx| this.show_left_tab(tab, window, cx)),
                )
                .child(label)
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .px_2()
            .pt_2()
            .pb(px(6.0))
            .child(tab("left-tab-files", "Files", LeftTab::Files, cx))
            .child(tab("left-tab-comments", "Comments", LeftTab::Comments, cx))
            .into_any_element()
    }

    /// VS Code's activity bar: one icon per left-dock view, settings at the
    /// bottom. Clicking the shown view hides the dock, as in VS Code.
    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let dock_visible = self.settings.file_tree.visible;
        let item = |id: &'static str, icon_name: IconName, tooltip: String, active: bool| {
            let color = if active {
                colors.text
            } else {
                colors.text_muted
            };
            div()
                .id(id)
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(ACTIVITY_BAR_WIDTH)
                .cursor_pointer()
                .tooltip(text_tooltip(tooltip))
                .child(icon(icon_name, ACTIVITY_ICON_SIZE, color))
                .when(active, |item| {
                    item.child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .w(px(2.0))
                            .bg(colors.accent),
                    )
                })
                .when(!active, |item| {
                    item.opacity(0.8).hover(|style| style.opacity(1.0))
                })
        };
        let view = |id: &'static str,
                    icon_name: IconName,
                    tooltip: String,
                    tab: LeftTab,
                    cx: &mut Context<Self>| {
            let active = dock_visible && self.left_tab == tab;
            item(id, icon_name, tooltip, active).on_click(cx.listener(
                move |this, _, window, cx| {
                    if active {
                        this.update_settings(window, cx, |settings| {
                            settings.file_tree.visible = false;
                        });
                    } else {
                        this.show_left_tab(tab, window, cx);
                    }
                },
            ))
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .justify_between()
            .h_full()
            .w(ACTIVITY_BAR_WIDTH)
            .bg(colors.surface)
            .border_r_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(view(
                        "activity-files",
                        IconName::Files,
                        with_shortcut::<ToggleFileTree>("Files"),
                        LeftTab::Files,
                        cx,
                    ))
                    .child(view(
                        "activity-comments",
                        IconName::ChatText,
                        with_shortcut::<ToggleCommentList>("Comments"),
                        LeftTab::Comments,
                        cx,
                    )),
            )
            .child(
                item(
                    "activity-settings",
                    IconName::GearSix,
                    with_shortcut::<OpenSettings>("Settings"),
                    self.sheet == Some(Sheet::Settings),
                )
                .on_click(|_, window, cx| window.dispatch_action(OpenSettings.boxed_clone(), cx)),
            )
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
            .child(self.run_panel.clone())
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
        let active = self
            .active_proposal
            .and_then(|index| self.proposals.get(index).map(|tab| (index, tab)));
        let content = match (active, self.session.clone()) {
            (Some((_, tab)), _) => tab.view.clone().into_any_element(),
            (None, Some(session)) => self.render_comparison(session.read(cx), cx),
            (None, None) => self.render_empty_state(cx).into_any_element(),
        };
        let active_index = active.map(|(index, _)| index);
        let tab = |id: gpui::ElementId,
                   title: SharedString,
                   is_active: bool,
                   target: Option<usize>,
                   cx: &mut Context<Self>| {
            div()
                .id(id)
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .h_full()
                .px_3()
                .max_w(px(320.0))
                .border_r_1()
                .border_color(colors.border)
                .text_sm()
                .cursor_pointer()
                .when(is_active, |tab| tab.bg(colors.background))
                .when(!is_active, |tab| {
                    tab.hover(|style| style.bg(colors.element_hover))
                })
                .text_color(if is_active {
                    colors.text
                } else {
                    colors.text_muted
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.activate_tab(target, cx);
                    if target.is_none() {
                        window.focus(&this.focus_handle, cx);
                    }
                }))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(title),
                )
                // Active-tab marker.
                .when(is_active, |tab| {
                    tab.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .h(px(2.0))
                            .bg(colors.accent),
                    )
                })
        };
        let main_tab = tab(
            "center-tab-main".into(),
            tab_title,
            active_index.is_none(),
            None,
            cx,
        );
        let proposal_tabs: Vec<AnyElement> = self
            .proposals
            .iter()
            .enumerate()
            .map(|(index, proposal)| {
                let title = proposal.view.read(cx).title();
                let close = div()
                    .id(("center-tab-close", index))
                    .flex_none()
                    .px_1()
                    .rounded_sm()
                    .text_color(colors.text_muted)
                    .hover(|style| style.bg(colors.element_selected))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_proposal(index, cx);
                    }))
                    .child("×");
                tab(
                    ("center-tab-proposal", index).into(),
                    title,
                    active_index == Some(index),
                    Some(index),
                    cx,
                )
                .child(close)
                .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(colors.background)
            // Tab bar: the main diff, then one tab per open proposal. With
            // the diff alone there is nothing to switch to, so no bar.
            .when(!proposal_tabs.is_empty(), |center| {
                center.child(
                    div()
                        .id("center-tabs")
                        .flex()
                        .flex_none()
                        .h(if self.is_vscode() {
                            VSCODE_HEADER_HEIGHT
                        } else {
                            px(32.0)
                        })
                        .overflow_x_scroll()
                        .bg(colors.surface)
                        .border_b_1()
                        .border_color(colors.border)
                        .child(main_tab)
                        .children(proposal_tabs),
                )
            })
            .child(content)
    }

    /// The center of an open repository: the resolved comparison, an inline
    /// error when the pair cannot be diffed, or what to pick next.
    fn render_comparison(&self, session: &RepoSession, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let path = display_path(&session.record().path);
        match session.comparison() {
            Comparison::Ready(_) => match &self.diff_view {
                Some(view) => view.clone().into_any_element(),
                None => placeholder("Opening the diff…", cx).into_any_element(),
            },
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
                        .rounded(radius::sm(cx))
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

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let message = |text: SharedString, color: gpui::Hsla| {
            div()
                .py_1()
                .text_sm()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let recent: AnyElement =
            match &self.recent {
                RecentRepos::Loading => message(
                    "Loading recent repositories…".into(),
                    colors.text_placeholder,
                ),
                RecentRepos::Failed(error) => message(
                    format!("Could not load recent repositories: {error}").into(),
                    colors.error,
                ),
                RecentRepos::Loaded(repos) if repos.is_empty() => {
                    message("No recent repositories.".into(), colors.text_placeholder)
                }
                // The rows' padding hangs into the margin so their text lines up
                // with the column while the hover fill keeps some room.
                RecentRepos::Loaded(repos) => div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .mx(px(-8.0))
                    .children(repos.iter().take(RECENT_REPOS_SHOWN).enumerate().map(
                        |(index, repo)| {
                            let path = repo.path.clone();
                            div()
                                .id(("recent-repo", index))
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .h(px(28.0))
                                .px_2()
                                .rounded(radius::sm(cx))
                                .text_sm()
                                .cursor_pointer()
                                .hover(|style| style.bg(colors.element_hover))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_path(path.clone(), cx)
                                }))
                                .child(kbd((index + 1).to_string(), cx))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(colors.text)
                                        .child(repo.name.clone()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_color(colors.text_placeholder)
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
                    .items_start()
                    .gap_6()
                    .w(px(420.0))
                    .p_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_3()
                            .child(eye_mark(px(WELCOME_MARK_SIZE), cx))
                            .child(
                                div()
                                    .text_size(px(WELCOME_WORDMARK_SIZE))
                                    .line_height(px(WELCOME_WORDMARK_SIZE * 1.2))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child("Argus"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                primary_button_with_icon(
                                    "open-repo",
                                    IconName::FolderSimple,
                                    "Open repository",
                                    cx,
                                )
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(OpenRepository.boxed_clone(), cx)
                                }),
                            )
                            .child(kbd(shortcut_text::<OpenRepository>(), cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .w_full()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(colors.text_muted)
                                    .child("Recent"),
                            )
                            .child(recent),
                    ),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let jobs = cx.global::<BackgroundJobs>().clone();
        let job_indicator = jobs.is_busy().then(|| {
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(colors.text_muted)
                .child(jobs.labels().join(" · "))
        });
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

        let dock_visible = self.settings.file_tree.visible;
        let files = toggle_icon_button(
            "status-files",
            IconName::SidebarSimple,
            with_shortcut::<ToggleFileTree>("Toggle file tree"),
            dock_visible && self.left_tab == LeftTab::Files,
            cx,
        )
        .on_click(|_, window, cx| window.dispatch_action(ToggleFileTree.boxed_clone(), cx));
        let comments = toggle_icon_button(
            "status-comments",
            IconName::ChatText,
            with_shortcut::<ToggleCommentList>("Comments"),
            dock_visible && self.left_tab == LeftTab::Comments,
            cx,
        )
        .on_click(
            cx.listener(|this, _, window, cx| this.show_left_tab(LeftTab::Comments, window, cx)),
        );
        let divider = div()
            .flex_none()
            .w(px(1.0))
            .h(px(14.0))
            .mx_1()
            .bg(colors.border_variant);

        let counters = self.comment_list.as_ref().map(|list| {
            let list = list.read(cx);
            let open = list.status_count(CommentStatus::Open);
            let running = list.status_count(CommentStatus::Running);
            let proposed = list.status_count(CommentStatus::Proposed);
            let mut counters: Vec<AnyElement> = Vec::new();
            if open > 0 {
                counters.push(
                    self.status_counter(
                        CommentStatus::Open,
                        icon(IconName::ChatText, STATUS_ICON_SIZE, colors.text_muted)
                            .into_any_element(),
                        format!("{open} open"),
                        colors.text_muted,
                        "Show open comments",
                        cx,
                    ),
                );
            }
            if running > 0 {
                counters.push(self.status_counter(
                    CommentStatus::Running,
                    claude_activity(STATUS_ICON_SIZE, cx),
                    format!("{running} running"),
                    colors.claude_text,
                    "Show running comments",
                    cx,
                ));
            }
            if proposed > 0 {
                counters.push(
                    self.status_counter(
                        CommentStatus::Proposed,
                        icon(IconName::GitPullRequest, STATUS_ICON_SIZE, colors.accent)
                            .into_any_element(),
                        format!("{proposed} to review"),
                        colors.accent,
                        "Show proposals to review",
                        cx,
                    ),
                );
            }
            counters
        });

        let waiting = (!self.permissions.is_empty()).then(|| {
            let count = self.permissions.len();
            div()
                .id("status-permissions")
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(20.0))
                .px_1p5()
                .rounded(radius::sm(cx))
                .cursor_pointer()
                .text_color(colors.claude_text)
                .bg(colors.claude_soft)
                .hover(|style| style.bg(colors.element_hover))
                .tooltip(text_tooltip("Answer the runs waiting for you"))
                .child(claude_activity(STATUS_ICON_SIZE, cx))
                .child(format!("{count} waiting for you"))
                .on_click(cx.listener(|this, _, _, cx| this.show_permissions(cx)))
        });

        // The resolved and dismissed threads the diff leaves out, when any.
        let closed = self.diff_view.as_ref().and_then(|view| {
            let view = view.read(cx);
            let count = view.closed_count();
            (count > 0).then(|| (count, view.shows_closed_threads()))
        });
        let closed = closed.map(|(count, shown)| {
            div()
                .id("status-closed")
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(20.0))
                .px_1p5()
                .rounded(radius::sm(cx))
                .cursor_pointer()
                .text_color(colors.text_placeholder)
                .when(shown, |counter| counter.bg(colors.element_selected))
                .hover(|style| style.bg(colors.element_hover))
                .tooltip(text_tooltip(if shown {
                    "Hide the resolved and dismissed comments"
                } else {
                    "Show the resolved and dismissed comments"
                }))
                .child(icon(
                    IconName::Check,
                    STATUS_ICON_SIZE,
                    colors.text_placeholder,
                ))
                .child(format!("{count} closed"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_main_diff(cx, |view, cx| view.toggle_closed_threads(cx));
                }))
        });

        // Diff display toggles: quick to reach while reading, out of the
        // way of the diff itself.
        let display_toggle = |id: &'static str,
                              label: &'static str,
                              on: bool,
                              tooltip: String,
                              action: Box<dyn Action>| {
            toggle_button(id, label, on, cx)
                .h(px(20.0))
                .px_1p5()
                .text_size(STATUS_TEXT_SIZE)
                .tooltip(text_tooltip(tooltip))
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        };
        let split = display_toggle(
            "status-split",
            "Split",
            self.is_split(),
            with_shortcut::<ToggleDiffMode>("Side by side"),
            ToggleDiffMode.boxed_clone(),
        );
        let wrap = display_toggle(
            "status-wrap",
            "Wrap",
            self.settings.diff_soft_wrap,
            with_shortcut::<ToggleSoftWrap>("Soft wrap"),
            ToggleSoftWrap.boxed_clone(),
        );

        let run_panel = toggle_icon_button(
            "status-runs",
            IconName::Sidebar,
            with_shortcut::<ToggleRunPanel>("Toggle run panel"),
            self.settings.run_panel.visible,
            cx,
        )
        .on_click(|_, window, cx| window.dispatch_action(ToggleRunPanel.boxed_clone(), cx));

        // VS Code moves the dock toggles and the settings to the activity
        // bar, and opens the bar on its accent-filled remote indicator.
        let vscode = self.is_vscode();
        let indicator = vscode.then(|| {
            div()
                .id("status-open-repository")
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .h_full()
                .px(px(9.0))
                .mr_1()
                .cursor_pointer()
                .bg(colors.accent)
                .hover(|style| style.opacity(0.9))
                .tooltip(text_tooltip(with_shortcut::<OpenRepository>(
                    "Open Repository…",
                )))
                .child(icon(
                    IconName::ArgusEyeMono,
                    STATUS_ICON_SIZE,
                    colors.accent_text,
                ))
                .on_click(|_, window, cx| window.dispatch_action(OpenRepository.boxed_clone(), cx))
        });

        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap_2()
            .h(self.status_bar_height())
            .when(vscode, |bar| bar.pr_1())
            .when(!vscode, |bar| bar.px_1())
            .bg(colors.status_bar)
            .border_t_1()
            .border_color(colors.border_variant)
            .text_size(STATUS_TEXT_SIZE)
            .text_color(colors.text_muted)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .min_w_0()
                    .h_full()
                    .children(indicator)
                    .when(!vscode, |bar| {
                        bar.child(files).child(comments).child(divider)
                    })
                    .children(waiting)
                    .children(counters.into_iter().flatten())
                    .children(closed)
                    .children(notice.map(|notice| notice.ml_1())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .min_w_0()
                    .children(job_indicator.map(|jobs| jobs.mr_1()))
                    .when(self.diff_view.is_some(), |bar| bar.child(split).child(wrap))
                    .when(!vscode, |bar| {
                        bar.child(
                            icon_button(
                                "status-settings",
                                IconName::SlidersHorizontal,
                                with_shortcut::<OpenSettings>("Settings"),
                                cx,
                            )
                            .on_click(|_, window, cx| {
                                window.dispatch_action(OpenSettings.boxed_clone(), cx)
                            }),
                        )
                    })
                    .child(
                        icon_button(
                            "status-shortcuts",
                            IconName::Keyboard,
                            with_shortcut::<ToggleKeyboardShortcuts>("Keyboard shortcuts"),
                            cx,
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(ToggleKeyboardShortcuts.boxed_clone(), cx)
                        }),
                    )
                    .child(run_panel),
            )
    }

    /// A status-bar lifecycle counter: a small ghost button that opens the
    /// Comments tab filtered to `status` only.
    fn status_counter(
        &self,
        status: CommentStatus,
        leading: AnyElement,
        label: String,
        color: gpui::Hsla,
        tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = &cx.theme().colors;
        div()
            .id(SharedString::from(format!("status-counter-{status}")))
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h(px(20.0))
            .px_1p5()
            .rounded(radius::sm(cx))
            .cursor_pointer()
            .text_color(color)
            .hover(|style| style.bg(colors.element_hover))
            .active(|style| style.bg(colors.element_selected))
            .tooltip(text_tooltip(tooltip))
            .child(leading)
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.show_comments_with_status(status, window, cx)
            }))
            .into_any_element()
    }

    /// Opens the Comments tab with the list filtered to `status` only.
    fn show_comments_with_status(
        &mut self,
        status: CommentStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(list) = self.comment_list.clone() {
            list.update(cx, |list, cx| list.show_only(status, cx));
        }
        self.show_left_tab(LeftTab::Comments, window, cx);
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
                        Sheet::Settings => 640.0,
                    }))
                    .max_h(px(600.0))
                    .rounded(radius::md(cx))
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
                            .child(
                                kbd("Esc", cx)
                                    .id("sheet-close")
                                    .cursor_pointer()
                                    .hover(|style| style.bg(colors.element_hover))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sheet = None;
                                        cx.notify();
                                    })),
                            ),
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
                        .rounded(radius::md(cx))
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
                        .filter_map(|shortcut| Some((shortcut, shortcut.sheet_keys()?)))
                        .map(|(shortcut, keys)| {
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .py_0p5()
                                .text_sm()
                                .text_color(colors.text)
                                .child(shortcut.description)
                                .child(kbd(keys, cx))
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
                .rounded(radius::sm(cx))
                .border_1()
                .border_color(colors.border_variant)
                .children(children)
                .into_any_element()
        };
        let stepper =
            |id: &'static str, value: f32, unit: &'static str, apply: fn(&mut UiSettings, f32)| {
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
                            .child(format!("{value:.0} {unit}")),
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

        let style_buttons = UiStyle::ALL
            .into_iter()
            .map(|style| {
                toggle_button(
                    ("ui-style", style as usize),
                    style.label(),
                    settings.ui_style == style,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_settings(window, cx, |settings| settings.ui_style = style)
                }))
                .into_any_element()
            })
            .collect();
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
        let diff_mode_buttons = DiffViewMode::ALL
            .into_iter()
            .map(|mode| {
                toggle_button(
                    ("diff-view-mode", mode as usize),
                    mode.label(),
                    settings.diff_view_mode == mode,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_settings(window, cx, |settings| settings.diff_view_mode = mode)
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
        // The tree owns the mode and reports it back; the buttons only ask
        // it to switch.
        let flat = settings.file_tree.flat_list;
        let file_list_buttons = [("Tree", false), ("List", true)]
            .into_iter()
            .map(|(label, is_flat)| {
                toggle_button(
                    ("file-list-mode", usize::from(is_flat)),
                    label,
                    flat == is_flat,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    if flat != is_flat {
                        window.dispatch_action(ToggleFileListMode.boxed_clone(), cx);
                    }
                })
                .into_any_element()
            })
            .collect();
        let visibility = |id: &'static str, visible: bool, action: Box<dyn Action>| {
            toggle_button(id, if visible { "Shown" } else { "Hidden" }, visible, cx)
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
                .into_any_element()
        };

        let reset = |id: &'static str, reset: fn(&mut UiSettings)| {
            button(id, "Reset to defaults", cx).on_click(
                cx.listener(move |this, _, window, cx| this.update_settings(window, cx, reset)),
            )
        };

        let diff = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_header(
                "Diff",
                reset("reset-diff-settings", UiSettings::reset_diff),
                cx,
            ))
            .child(row("Layout", segmented(diff_mode_buttons)))
            .child(row(
                "Tab width",
                stepper(
                    "diff-tab-width",
                    settings.diff_tab_width as f32,
                    "cols",
                    |settings, delta| {
                        let width = settings.diff_tab_width as f32 + delta;
                        settings.diff_tab_width = clamp_tab_width(width.max(0.0) as u32);
                    },
                ),
            ))
            .child(row(
                "Horizontal scroll",
                toggle_button(
                    "diff-link-scroll-setting",
                    if settings.diff_link_horizontal_scroll {
                        "Linked"
                    } else {
                        "Per column"
                    },
                    settings.diff_link_horizontal_scroll,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.update_settings(window, cx, |settings| {
                        settings.diff_link_horizontal_scroll =
                            !settings.diff_link_horizontal_scroll;
                    })
                }))
                .into_any_element(),
            ))
            .child(row(
                "Soft wrap",
                toggle_button(
                    "diff-soft-wrap-setting",
                    if settings.diff_soft_wrap { "On" } else { "Off" },
                    settings.diff_soft_wrap,
                    cx,
                )
                .on_click(|_, window, cx| window.dispatch_action(ToggleSoftWrap.boxed_clone(), cx))
                .into_any_element(),
            ));

        let hidden_rows: Vec<AnyElement> = settings
            .hidden_files
            .iter()
            .enumerate()
            .map(|(index, pattern)| {
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pl_2()
                    .rounded(radius::sm(cx))
                    .bg(colors.surface)
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_family(theme.fonts.code_family.clone())
                            .text_sm()
                            .text_color(colors.text)
                            .child(pattern.clone()),
                    )
                    .child(
                        icon_button(("hidden-file-remove", index), IconName::X, "Remove", cx)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let mut patterns = this.settings.hidden_files.clone();
                                if index < patterns.len() {
                                    patterns.remove(index);
                                }
                                this.set_hidden_file_patterns(patterns, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        let hidden = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_header(
                "Hidden files",
                div().when(!settings.hidden_files.is_empty(), |trailing| {
                    trailing.child(button("clear-hidden-files", "Show all files", cx).on_click(
                        cx.listener(|this, _, _, cx| this.set_hidden_file_patterns(Vec::new(), cx)),
                    ))
                }),
                cx,
            ))
            .child(self.hidden_file_input.clone())
            .children(
                self.hidden_file_error
                    .clone()
                    .map(|error| div().text_sm().text_color(colors.error).child(error)),
            )
            .children(hidden_rows);

        let appearance = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_header(
                "Appearance",
                reset("reset-appearance-settings", UiSettings::reset_appearance),
                cx,
            ))
            .child(row("UI style", segmented(style_buttons)))
            .child(row("Theme", segmented(theme_buttons)))
            .child(row("Active theme", muted(theme.name.clone())))
            .child(row(
                "UI font size",
                stepper(
                    "ui-font-size",
                    settings.ui_font_size,
                    "px",
                    |settings, delta| {
                        settings.ui_font_size = clamp_font_size(settings.ui_font_size + delta, 0.0);
                    },
                ),
            ))
            .child(row(
                "Code font size",
                stepper(
                    "code-font-size",
                    settings.code_font_size,
                    "px",
                    |settings, delta| {
                        settings.code_font_size =
                            clamp_font_size(settings.code_font_size + delta, 0.0);
                    },
                ),
            ))
            .child(row("UI font", muted(theme.fonts.ui_family.clone())))
            .child(row("Code font", muted(theme.fonts.code_family.clone())))
            .child(row(
                "Reduce motion",
                toggle_button(
                    "reduce-motion-setting",
                    if settings.reduce_motion { "On" } else { "Off" },
                    settings.reduce_motion,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.update_settings(window, cx, |settings| {
                        settings.reduce_motion = !settings.reduce_motion;
                    })
                }))
                .into_any_element(),
            ));

        let layout = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_header(
                "Layout",
                reset("reset-layout-settings", UiSettings::reset_layout),
                cx,
            ))
            .child(row(
                "File tree",
                visibility(
                    "file-tree-visible",
                    settings.file_tree.visible,
                    ToggleFileTree.boxed_clone(),
                ),
            ))
            .child(row("File list", segmented(file_list_buttons)))
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
            ));

        let git = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_header("Git", div(), cx))
            .child(row(
                "Background fetch",
                toggle_button(
                    "background-fetch-setting",
                    if settings.background_fetch {
                        "On"
                    } else {
                        "Off"
                    },
                    settings.background_fetch,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.update_settings(window, cx, |settings| {
                        settings.background_fetch = !settings.background_fetch;
                    })
                }))
                .into_any_element(),
            ));

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.claude_settings.clone())
            .child(diff)
            .child(hidden)
            .child(appearance)
            .child(layout)
            .child(git)
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        perf::frame_started(cx);
        if std::mem::take(&mut self.focus_requested) {
            window.focus(&self.focus_handle, cx);
        }
        let theme = cx.theme().clone();
        // Rem-based spacing and text sizes scale with the UI font size, as in
        // Zed.
        window.set_rem_size(theme.fonts.ui_size);
        // `Auto` follows the room the diff gets: a window resize or a dock
        // shown, hidden or dragged can switch the layout.
        let was_split = self.is_split();
        self.diff_area_width = self.center_width(window.viewport_size().width.as_f32());
        if self.is_split() != was_split {
            self.sync_diff_options(cx);
        }

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
            .when(self.is_vscode(), |body| {
                body.child(self.render_activity_bar(cx))
            })
            .when(self.settings.file_tree.visible, |body| {
                body.child(self.render_file_tree(cx))
            })
            .child(main_column)
            .when(run_panel_right, |body| {
                body.child(self.render_run_panel(cx))
            });

        div()
            .id("workspace")
            .key_context({
                let mut context = gpui::KeyContext::default();
                context.add(WORKSPACE_CONTEXT);
                if self.is_welcome_shown() {
                    context.add(WELCOME_CONTEXT);
                }
                context
            })
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
            .on_action(cx.listener(|this, _: &OpenRecent1, _, cx| this.open_recent(0, cx)))
            .on_action(cx.listener(|this, _: &OpenRecent2, _, cx| this.open_recent(1, cx)))
            .on_action(cx.listener(|this, _: &OpenRecent3, _, cx| this.open_recent(2, cx)))
            .on_action(cx.listener(|this, _: &OpenRecent4, _, cx| this.open_recent(3, cx)))
            .on_action(cx.listener(|this, _: &OpenRecent5, _, cx| this.open_recent(4, cx)))
            .on_action(cx.listener(|this, _: &OpenRecent6, _, cx| this.open_recent(5, cx)))
            .on_action(cx.listener(|this, _: &OpenRecent7, _, cx| this.open_recent(6, cx)))
            .on_action(cx.listener(|this, _: &OpenRecent8, _, cx| this.open_recent(7, cx)))
            .on_action(cx.listener(|this, _: &OpenRecent9, _, cx| this.open_recent(8, cx)))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(|this, _: &CheckForUpdates, _, cx| {
                this.updater
                    .update(cx, |updater, cx| updater.check(true, cx));
            }))
            .on_action(cx.listener(Self::toggle_shortcuts))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::toggle_file_tree))
            .on_action(cx.listener(Self::toggle_run_panel))
            .on_action(cx.listener(Self::toggle_run_panel_position))
            .on_action(cx.listener(Self::toggle_focus_mode))
            .on_action(cx.listener(Self::cycle_theme))
            .on_action(cx.listener(Self::increase_font_size))
            .on_action(cx.listener(Self::decrease_font_size))
            .on_action(cx.listener(Self::reset_font_size))
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::previous_file))
            .on_action(cx.listener(Self::focus_file_filter))
            .on_action(cx.listener(Self::toggle_file_list_mode))
            .on_action(cx.listener(Self::next_hunk))
            .on_action(cx.listener(Self::previous_hunk))
            .on_action(cx.listener(Self::toggle_soft_wrap))
            .on_action(cx.listener(Self::toggle_diff_mode))
            .on_action(cx.listener(Self::comment))
            .on_action(cx.listener(Self::next_comment))
            .on_action(cx.listener(Self::previous_comment))
            .on_action(cx.listener(Self::toggle_all_comments))
            .on_action(cx.listener(Self::toggle_comment_list))
            .on_action(cx.listener(Self::advance_comment_status))
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::explain))
            .on_action(cx.listener(Self::open_diagram))
            .on_action(cx.listener(Self::accept))
            .child(self.render_title_bar(cx))
            .children(self.render_claude_banner(cx))
            .child(body)
            .child(self.render_status_bar(cx))
            .when_some(self.sheet, |root, sheet| {
                root.child(self.render_sheet(sheet, cx))
            })
            .children(self.render_tracking_dialog(cx))
            .children(
                self.run_panel
                    .update(cx, |panel, cx| panel.render_dialog(cx)),
            )
            .when(self.accept_flow.read(cx).has_dialog(), |root| {
                root.child(self.accept_flow.clone())
            })
            .when(self.push_flow.read(cx).is_visible(), |root| {
                root.child(self.push_flow.clone())
            })
            .when_some(
                self.conflict.as_ref().map(|(dialog, _)| dialog.clone()),
                |root, dialog| root.child(dialog),
            )
            .when_some(
                self.explanation.as_ref().map(|(dialog, _)| dialog.clone()),
                |root, dialog| root.child(dialog),
            )
            .when_some(
                self.diagram_viewer
                    .as_ref()
                    .map(|(viewer, _)| viewer.clone()),
                |root, viewer| root.child(viewer),
            )
            // Over every other dialog: a run is blocked until it is answered.
            .when_some(
                self.permission_dialog
                    .as_ref()
                    .map(|(dialog, _)| dialog.clone()),
                |root, dialog| root.child(dialog),
            )
            // Last, so its paint ends the `ARGUS_PERF` frame measurement.
            .children(perf::frame_marker(cx))
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

/// `label` followed by the shortcut of action `A` in parentheses, for
/// tooltips; just `label` when the action has no binding.
fn with_shortcut<A: Action>(label: &str) -> String {
    let keys = shortcut_text::<A>();
    if keys.is_empty() {
        label.to_owned()
    } else {
        format!("{label} ({keys})")
    }
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
pub(crate) fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}
