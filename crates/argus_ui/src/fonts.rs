//! Font family resolution.
//!
//! GPUI does not walk a font's fallback list when the primary family is
//! missing; it drops to a global sans-serif stack instead, which would render
//! code in a proportional font. So Argus picks the first installed family from
//! a preference list once, at startup or when the setting changes.
//!
//! IBM Plex Sans and IBM Plex Mono are compiled into the binary (SIL Open
//! Font License 1.1, see `assets/fonts/OFL.txt`) and registered once at
//! startup by [`register_bundled_fonts`], so they head both preference lists
//! and Argus looks the same on every machine.

use std::borrow::Cow;

use gpui::{App, SharedString};

/// The bundled UI family.
pub const BUNDLED_UI_FAMILY: &str = "IBM Plex Sans";

/// The bundled code family.
pub const BUNDLED_CODE_FAMILY: &str = "IBM Plex Mono";

/// IBM Plex Sans Regular, also the font of Mermaid diagram labels.
pub(crate) const PLEX_SANS_REGULAR: &[u8] =
    include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf");

/// The bundled font files, exactly as released by IBM.
const BUNDLED_FONTS: &[&[u8]] = &[
    PLEX_SANS_REGULAR,
    include_bytes!("../assets/fonts/IBMPlexSans-Medium.ttf"),
    include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf"),
    include_bytes!("../assets/fonts/IBMPlexMono-Medium.ttf"),
    include_bytes!("../assets/fonts/IBMPlexMono-Italic.ttf"),
];

/// Registers the bundled IBM Plex fonts with the text system.
///
/// The bytes are compiled in, so this does no file I/O. On failure Argus
/// keeps going and the preference lists fall back to installed fonts.
pub fn register_bundled_fonts(cx: &App) {
    let fonts = BUNDLED_FONTS
        .iter()
        .map(|bytes| Cow::Borrowed(*bytes))
        .collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "failed to register the bundled IBM Plex fonts");
    }
}

/// Preferred code fonts: the bundled IBM Plex Mono first, then Zed's default
/// and common system monospace fonts on macOS and Linux.
const CODE_FONT_PREFERENCES: &[&str] = &[
    BUNDLED_CODE_FAMILY,
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

/// Preferred UI fonts: the bundled IBM Plex Sans first. `.SystemUIFont` is
/// GPUI's alias for the platform UI font and always resolves, so it ends the
/// list.
const UI_FONT_PREFERENCES: &[&str] = &[BUNDLED_UI_FAMILY, "Zed Sans", "Inter", ".SystemUIFont"];

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

    /// The code font: the configured family if installed, else the bundled
    /// IBM Plex Mono or the first installed fallback monospace font.
    pub fn code_family(&self, user_choice: Option<&str>) -> SharedString {
        self.pick(user_choice, CODE_FONT_PREFERENCES)
    }

    /// The UI font: the configured family if installed, else the bundled
    /// IBM Plex Sans or the first installed fallback UI font.
    pub fn ui_family(&self, user_choice: Option<&str>) -> SharedString {
        self.pick(user_choice, UI_FONT_PREFERENCES)
    }
}
