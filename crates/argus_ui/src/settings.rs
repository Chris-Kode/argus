//! UI settings persisted in the `settings` table under the [`UI_SETTINGS_KEY`]
//! key: theme, fonts, and dock sizes and visibility.
//!
//! Loading and saving go through [`argus_store::SettingsStore`] on the tokio
//! runtime, never on the UI thread.

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
    /// UI font family override; `None` picks Zed's UI font or the system one.
    pub ui_font_family: Option<String>,
    /// Code font family override; `None` picks "Zed Mono" or the system
    /// monospace font.
    pub code_font_family: Option<String>,
    /// UI font size in pixels.
    pub ui_font_size: f32,
    /// Code font size in pixels.
    pub code_font_size: f32,
    /// File tree dock.
    pub file_tree: FileTreeDock,
    /// Run panel dock.
    pub run_panel: RunPanelDock,
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

    /// Clamps values a hand-edited or corrupted record could put out of range.
    pub fn sanitized(mut self) -> Self {
        self.ui_font_size = clamp_font_size(self.ui_font_size, DEFAULT_UI_FONT_SIZE);
        self.code_font_size = clamp_font_size(self.code_font_size, DEFAULT_CODE_FONT_SIZE);
        self.file_tree.width = clamp_dock_size(self.file_tree.width, DEFAULT_FILE_TREE_WIDTH);
        self.run_panel.width = clamp_dock_size(self.run_panel.width, DEFAULT_RUN_PANEL_WIDTH);
        self.run_panel.height = clamp_dock_size(self.run_panel.height, DEFAULT_RUN_PANEL_HEIGHT);
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

fn clamp_dock_size(size: f32, default: f32) -> f32 {
    if size.is_finite() {
        size.max(MIN_DOCK_SIZE)
    } else {
        default
    }
}
