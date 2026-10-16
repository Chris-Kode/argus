//! Font family resolution.
//!
//! GPUI does not walk a font's fallback list when the primary family is
//! missing; it drops to a global sans-serif stack instead, which would render
//! code in a proportional font. So Argus picks the first installed family from
//! a preference list once, at startup or when the setting changes.

use gpui::{App, SharedString};

/// Preferred code fonts: Zed's default first, then common system monospace
/// fonts on macOS and Linux.
const CODE_FONT_PREFERENCES: &[&str] = &[
    "Zed Mono",
    "Lilex",
    "SF Mono",
    "Menlo",
    "JetBrains Mono",
    "Cascadia Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Noto Mono",
    "Ubuntu Mono",
    "Nimbus Mono PS",
    "Courier New",
];

/// Preferred UI fonts. `.SystemUIFont` is GPUI's alias for the platform UI
/// font and always resolves, so it ends the list.
const UI_FONT_PREFERENCES: &[&str] = &["Zed Sans", "IBM Plex Sans", "Inter", ".SystemUIFont"];

/// Installed font families, listed once and reused for every resolution.
pub struct InstalledFonts {
    names: Vec<String>,
}

impl InstalledFonts {
    /// Lists the families known to the platform text system.
    pub fn load(cx: &App) -> Self {
        Self {
            names: cx.text_system().all_font_names(),
        }
    }

    fn has(&self, family: &str) -> bool {
        family.starts_with('.') || self.names.iter().any(|name| name == family)
    }

    fn pick(&self, user_choice: Option<&str>, preferences: &[&'static str]) -> SharedString {
        if let Some(choice) = user_choice.filter(|choice| self.has(choice)) {
            return SharedString::from(choice.to_owned());
        }
        if let Some(choice) = user_choice {
            tracing::warn!(
                font = choice,
                "configured font is not installed; using a fallback"
            );
        }
        preferences
            .iter()
            .copied()
            .find(|family| self.has(family))
            .map(SharedString::new_static)
            .unwrap_or_else(|| SharedString::new_static(preferences[0]))
    }

    /// The code font: the configured family if installed, else "Zed Mono" or
    /// the first installed system monospace font.
    pub fn code_family(&self, user_choice: Option<&str>) -> SharedString {
        self.pick(user_choice, CODE_FONT_PREFERENCES)
    }

    /// The UI font: the configured family if installed, else Zed's UI font or
    /// the platform UI font.
    pub fn ui_family(&self, user_choice: Option<&str>) -> SharedString {
        self.pick(user_choice, UI_FONT_PREFERENCES)
    }
}
