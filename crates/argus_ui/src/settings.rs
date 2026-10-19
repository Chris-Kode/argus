//! Settings persisted in the `settings` table (spec §10).
//!
//! [`Settings`] bundles the two typed records: [`UiSettings`] under the
//! [`UI_SETTINGS_KEY`] key (theme, fonts, dock sizes and visibility, diff
//! display options) and [`ClaudeSettings`] under
//! [`argus_runs::settings::CLAUDE_SETTINGS_KEY`] (the run defaults). Both
//! default every missing field and ignore unknown ones, so records written by
//! older or newer versions keep loading.
//!
//! Loading and saving go through [`argus_store::SettingsStore`] on the tokio
//! runtime, never on the UI thread.

use argus_runs::ClaudeSettings;
use argus_store::SettingsStore;
use serde::{Deserialize, Serialize};

use crate::theme::ThemeMode;

/// Settings key holding the JSON-encoded [`UiSettings`].
pub const UI_SETTINGS_KEY: &str = "ui";

/// Default UI font size, in pixels.
pub const DEFAULT_UI_FONT_SIZE: f32 = 14.0;
/// Default code font size, in pixels.
pub const DEFAULT_CODE_FONT_SIZE: f32 = 13.0;
/// Smallest allowed font size, in pixels.
pub const MIN_FONT_SIZE: f32 = 8.0;
/// Largest allowed font size, in pixels.
pub const MAX_FONT_SIZE: f32 = 32.0;

/// Smallest dock size along its resize axis, in pixels.
pub const MIN_DOCK_SIZE: f32 = 120.0;
/// Default width of the file tree dock.
pub const DEFAULT_FILE_TREE_WIDTH: f32 = 260.0;
/// Default width of the run panel when docked right.
pub const DEFAULT_RUN_PANEL_WIDTH: f32 = 340.0;
/// Default height of the run panel when docked at the bottom.
pub const DEFAULT_RUN_PANEL_HEIGHT: f32 = 240.0;

/// Default display width of a tab in the diff view, in columns.
pub const DEFAULT_TAB_WIDTH: u32 = 4;
/// Smallest allowed tab width.
pub const MIN_TAB_WIDTH: u32 = 1;
/// Largest allowed tab width.
pub const MAX_TAB_WIDTH: u32 = 16;

/// Seconds between two background fetches of the head's upstream.
pub const BACKGROUND_FETCH_INTERVAL_SECS: u64 = 60;

/// Every persisted setting, as loaded at startup.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// Run defaults; live in [`argus_runs::RunManager`] once the app runs.
    pub claude: ClaudeSettings,
    /// Appearance, layout and diff display; owned by the workspace.
    pub ui: UiSettings,
}

impl Settings {
    /// Loads both records, each falling back to its defaults when missing or
    /// unreadable. Must run on the tokio runtime.
    pub async fn load(store: &SettingsStore) -> Self {
        Self {
            claude: ClaudeSettings::load(store).await,
            ui: UiSettings::load(store).await,
        }
    }
}

/// Where the run panel is docked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPanelPosition {
    /// To the right of the diff view.
    #[default]
    Right,
    /// Below the diff view.
    Bottom,
}

impl RunPanelPosition {
    /// Human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            RunPanelPosition::Right => "Right",
            RunPanelPosition::Bottom => "Bottom",
        }
    }

    /// The other position.
    pub fn toggled(self) -> Self {
        match self {
            RunPanelPosition::Right => RunPanelPosition::Bottom,
            RunPanelPosition::Bottom => RunPanelPosition::Right,
        }
    }
}

/// The diff layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffViewMode {
    /// One column with old and new line numbers.
    Unified,
    /// Old and new columns side by side (spec §3.1 default).
    #[default]
    SideBySide,
}

impl DiffViewMode {
    /// Human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            DiffViewMode::Unified => "Unified",
            DiffViewMode::SideBySide => "Side by side",
        }
    }

    /// The other mode.
    pub fn toggled(self) -> Self {
        match self {
            DiffViewMode::Unified => DiffViewMode::SideBySide,
            DiffViewMode::SideBySide => DiffViewMode::Unified,
        }
    }
}

/// The left dock (file tree).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileTreeDock {
    /// Whether the dock is shown.
    pub visible: bool,
    /// Width in pixels.
    pub width: f32,
    /// Show a flat list of paths instead of the directory tree.
    pub flat_list: bool,
}

impl Default for FileTreeDock {
    fn default() -> Self {
        Self {
            visible: true,
            width: DEFAULT_FILE_TREE_WIDTH,
            flat_list: false,
        }
    }
}

/// The right/bottom dock (run panel).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunPanelDock {
    /// Whether the dock is shown.
    pub visible: bool,
    /// Right or bottom.
    pub position: RunPanelPosition,
    /// Width in pixels when docked right.
    pub width: f32,
    /// Height in pixels when docked at the bottom.
    pub height: f32,
}

impl Default for RunPanelDock {
    fn default() -> Self {
        Self {
            visible: true,
            position: RunPanelPosition::Right,
            width: DEFAULT_RUN_PANEL_WIDTH,
            height: DEFAULT_RUN_PANEL_HEIGHT,
        }
    }
}

/// Every persisted UI setting. Missing fields take their defaults, so older
/// stored values keep loading as fields are added.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// System, dark or light.
    pub theme: ThemeMode,
    /// UI font family override; `None` picks the bundled IBM Plex Sans.
    pub ui_font_family: Option<String>,
    /// Code font family override; `None` picks the bundled IBM Plex Mono.
    pub code_font_family: Option<String>,
    /// UI font size in pixels.
    pub ui_font_size: f32,
    /// Code font size in pixels.
    pub code_font_size: f32,
    /// File tree dock.
    pub file_tree: FileTreeDock,
    /// Run panel dock.
    pub run_panel: RunPanelDock,
    /// Diff view: tab width in columns.
    pub diff_tab_width: u32,
    /// Diff view: soft-wrap long lines instead of scrolling horizontally.
    pub diff_soft_wrap: bool,
    /// Diff view: unified or side-by-side; the mode new diffs open in.
    pub diff_view_mode: DiffViewMode,
    /// Diff view, side by side: one horizontal scroll for both columns
    /// instead of one per column.
    pub diff_link_horizontal_scroll: bool,
    /// Fetch the head's upstream every [`BACKGROUND_FETCH_INTERVAL_SECS`]
    /// seconds so the Push counts stay current (spec §5.4).
    pub background_fetch: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            ui_font_family: None,
            code_font_family: None,
            ui_font_size: DEFAULT_UI_FONT_SIZE,
            code_font_size: DEFAULT_CODE_FONT_SIZE,
            file_tree: FileTreeDock::default(),
            run_panel: RunPanelDock::default(),
            diff_tab_width: DEFAULT_TAB_WIDTH,
            diff_soft_wrap: false,
            diff_view_mode: DiffViewMode::default(),
            diff_link_horizontal_scroll: true,
            background_fetch: true,
        }
    }
}

impl UiSettings {
    /// Loads the settings, falling back to defaults when they are missing or
    /// unreadable. Must run on the tokio runtime.
    pub async fn load(store: &SettingsStore) -> Self {
        match store.get::<UiSettings>(UI_SETTINGS_KEY).await {
            Ok(Some(settings)) => settings.sanitized(),
            Ok(None) => Self::default(),
            Err(error) => {
                tracing::warn!(%error, "failed to load UI settings; using defaults");
                Self::default()
            }
        }
    }

    /// Stores the settings. Must run on the tokio runtime.
    pub async fn save(&self, store: &SettingsStore) -> argus_store::Result<()> {
        store.set(UI_SETTINGS_KEY, self).await
    }

    /// Resets the Diff section: layout, tab width, soft wrap and scrolling.
    pub fn reset_diff(&mut self) {
        let defaults = Self::default();
        self.diff_tab_width = defaults.diff_tab_width;
        self.diff_soft_wrap = defaults.diff_soft_wrap;
        self.diff_view_mode = defaults.diff_view_mode;
        self.diff_link_horizontal_scroll = defaults.diff_link_horizontal_scroll;
    }

    /// Resets the Appearance section: theme and fonts.
    pub fn reset_appearance(&mut self) {
        let defaults = Self::default();
        self.theme = defaults.theme;
        self.ui_font_family = defaults.ui_font_family;
        self.code_font_family = defaults.code_font_family;
        self.ui_font_size = defaults.ui_font_size;
        self.code_font_size = defaults.code_font_size;
    }

    /// Resets the Layout section: dock visibility, position and sizes. The
    /// file tree's flat/tree mode is a view toggle and stays as is.
    pub fn reset_layout(&mut self) {
        let flat_list = self.file_tree.flat_list;
        self.file_tree = FileTreeDock {
            flat_list,
            ..FileTreeDock::default()
        };
        self.run_panel = RunPanelDock::default();
    }

    /// Clamps values a hand-edited or corrupted record could put out of range.
    pub fn sanitized(mut self) -> Self {
        self.ui_font_size = clamp_font_size(self.ui_font_size, DEFAULT_UI_FONT_SIZE);
        self.code_font_size = clamp_font_size(self.code_font_size, DEFAULT_CODE_FONT_SIZE);
        self.file_tree.width = clamp_dock_size(self.file_tree.width, DEFAULT_FILE_TREE_WIDTH);
        self.run_panel.width = clamp_dock_size(self.run_panel.width, DEFAULT_RUN_PANEL_WIDTH);
        self.run_panel.height = clamp_dock_size(self.run_panel.height, DEFAULT_RUN_PANEL_HEIGHT);
        self.diff_tab_width = clamp_tab_width(self.diff_tab_width);
        self
    }
}

/// Clamps a font size to the allowed range; non-finite values reset.
pub fn clamp_font_size(size: f32, default: f32) -> f32 {
    if size.is_finite() {
        size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    } else {
        default
    }
}

/// Clamps a tab width to `[MIN_TAB_WIDTH, MAX_TAB_WIDTH]`.
pub fn clamp_tab_width(width: u32) -> u32 {
    width.clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH)
}

fn clamp_dock_size(size: f32, default: f32) -> f32 {
    if size.is_finite() {
        size.max(MIN_DOCK_SIZE)
    } else {
        default
    }
}
