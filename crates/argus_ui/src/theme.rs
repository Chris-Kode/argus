//! Zed-style theme: One Dark and One Light color tokens plus the UI and code
//! fonts.
//!
//! The active [`Theme`] is a GPUI global. Views read it through
//! [`ActiveTheme::theme`] and never hard-code colors, so switching theme only
//! has to replace the global and refresh the windows.

use argus_highlight::HighlightKind;
use gpui::{
    App, FontStyle, FontWeight, Global, HighlightStyle, Hsla, Pixels, Rgba, SharedString,
    WindowAppearance, px, rgba,
};
use serde::{Deserialize, Serialize};

/// The user's theme choice, persisted in the settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Follow the operating system appearance.
    #[default]
    System,
    /// Always One Dark.
    Dark,
    /// Always One Light.
    Light,
}

impl ThemeMode {
    /// Every mode, in the order the settings sheet lists them.
    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Dark, ThemeMode::Light];

    /// Human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "System",
            ThemeMode::Dark => "Dark",
            ThemeMode::Light => "Light",
        }
    }

    /// The next mode in the System → Dark → Light cycle.
    pub fn next(self) -> Self {
        match self {
            ThemeMode::System => ThemeMode::Dark,
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::System,
        }
    }

    /// Resolves the mode to a concrete appearance, consulting the window
    /// appearance for [`ThemeMode::System`].
    pub fn resolve(self, window_appearance: WindowAppearance) -> Appearance {
        match self {
            ThemeMode::Dark => Appearance::Dark,
            ThemeMode::Light => Appearance::Light,
            ThemeMode::System => match window_appearance {
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Appearance::Dark,
                WindowAppearance::Light | WindowAppearance::VibrantLight => Appearance::Light,
            },
        }
    }
}

/// A concrete light or dark appearance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    /// One Dark.
    Dark,
    /// One Light.
    Light,
}

/// Semantic color tokens. Views use these names, never raw colors.
#[derive(Clone, Debug)]
pub struct ThemeColors {
    /// Window and editor background.
    pub background: Hsla,
    /// Panels, docks and tab bars.
    pub surface: Hsla,
    /// Floating surfaces: sheets, popovers, dialogs.
    pub elevated_surface: Hsla,
    /// Title bar background.
    pub title_bar: Hsla,
    /// Status bar background.
    pub status_bar: Hsla,
    /// Default border.
    pub border: Hsla,
    /// Subtle separators inside a surface.
    pub border_variant: Hsla,
    /// Primary text.
    pub text: Hsla,
    /// Secondary text.
    pub text_muted: Hsla,
    /// Placeholder and disabled text.
    pub text_placeholder: Hsla,
    /// Accent color: focus, active tab marker, primary buttons.
    pub accent: Hsla,
    /// Text drawn on top of [`ThemeColors::accent`].
    pub accent_text: Hsla,
    /// Hovered interactive element.
    pub element_hover: Hsla,
    /// Selected or pressed interactive element.
    pub element_selected: Hsla,
    /// Backdrop dimming the window behind a sheet.
    pub overlay: Hsla,
    /// Background of an added diff line.
    pub diff_added_background: Hsla,
    /// Background of the changed words inside an added line.
    pub diff_added_word: Hsla,
    /// Foreground for "added" markers (+ counts, gutter).
    pub diff_added_text: Hsla,
    /// Background of a removed diff line.
    pub diff_removed_background: Hsla,
    /// Background of the changed words inside a removed line.
    pub diff_removed_word: Hsla,
    /// Foreground for "removed" markers (- counts, gutter).
    pub diff_removed_text: Hsla,
    /// Warnings (outdated comments, notices).
    pub warning: Hsla,
    /// Errors (failed runs, failed operations).
    pub error: Hsla,
    /// The "Proposed" comment status chip (a change is waiting for review).
    pub status_proposed: Hsla,
}

fn color(hex: u32) -> Hsla {
    let rgba: Rgba = rgba(hex);
    rgba.into()
}

impl ThemeColors {
    /// Zed's One Dark palette.
    pub fn one_dark() -> Self {
        Self {
            background: color(0x282c33ff),
            surface: color(0x2f343eff),
            elevated_surface: color(0x2f343eff),
            title_bar: color(0x3b414dff),
            status_bar: color(0x3b414dff),
            border: color(0x464b57ff),
            border_variant: color(0x363c46ff),
            text: color(0xdce0e5ff),
            text_muted: color(0xa9afbcff),
            text_placeholder: color(0x878a98ff),
            accent: color(0x74ade8ff),
            accent_text: color(0x282c33ff),
            element_hover: color(0x363c46ff),
            element_selected: color(0x454a56ff),
            overlay: color(0x0000008c),
            diff_added_background: color(0x222e1dff),
            diff_added_word: color(0xa7c08840),
            diff_added_text: color(0xa7c088ff),
            diff_removed_background: color(0x301b1bff),
            diff_removed_word: color(0xd0727740),
            diff_removed_text: color(0xd07277ff),
            warning: color(0xdec184ff),
            error: color(0xd07277ff),
            status_proposed: color(0xb477cfff),
        }
    }

    /// Zed's One Light palette.
    pub fn one_light() -> Self {
        Self {
            background: color(0xfafafaff),
            surface: color(0xebebecff),
            elevated_surface: color(0xebebecff),
            title_bar: color(0xdcdcddff),
            status_bar: color(0xdcdcddff),
            border: color(0xc9c9caff),
            border_variant: color(0xdfdfe0ff),
            text: color(0x242529ff),
            text_muted: color(0x58585aff),
            text_placeholder: color(0x7e8086ff),
            accent: color(0x5c78e2ff),
            accent_text: color(0xfafafaff),
            element_hover: color(0xdfdfe0ff),
            element_selected: color(0xc9c9caff),
            overlay: color(0x00000040),
            diff_added_background: color(0xdfeadbff),
            diff_added_word: color(0x669f5940),
            diff_added_text: color(0x669f59ff),
            diff_removed_background: color(0xfbdfd9ff),
            diff_removed_word: color(0xd3615140),
            diff_removed_text: color(0xd36151ff),
            warning: color(0xa48819ff),
            error: color(0xd36151ff),
            status_proposed: color(0xa449abff),
        }
    }
}

/// Syntax colors by [`HighlightKind`], from Zed's One Dark / One Light
/// `syntax` styles.
#[derive(Clone, Debug)]
pub struct SyntaxTheme {
    styles: [HighlightStyle; HighlightKind::COUNT],
}

impl SyntaxTheme {
    /// The text style of `kind`.
    pub fn style(&self, kind: HighlightKind) -> HighlightStyle {
        self.styles[kind.index()]
    }

    fn from_palette(palette: impl Fn(HighlightKind) -> u32) -> Self {
        Self {
            styles: HighlightKind::ALL.map(|kind| HighlightStyle {
                color: Some(color(palette(kind))),
                font_weight: (kind == HighlightKind::EmphasisStrong).then_some(FontWeight::BOLD),
                font_style: matches!(kind, HighlightKind::Emphasis | HighlightKind::LinkText)
                    .then_some(FontStyle::Italic),
                ..HighlightStyle::default()
            }),
        }
    }

    /// Zed's One Dark syntax palette.
    pub fn one_dark() -> Self {
        Self::from_palette(|kind| match kind {
            HighlightKind::Attribute => 0x74ade8ff,
            HighlightKind::Boolean => 0xbf956aff,
            HighlightKind::Comment => 0x5d636fff,
            HighlightKind::CommentDoc => 0x878e98ff,
            HighlightKind::Constant => 0xdfc184ff,
            HighlightKind::Constructor => 0x73ade9ff,
            HighlightKind::Embedded => 0xdce0e5ff,
            HighlightKind::Emphasis => 0x74ade8ff,
            HighlightKind::EmphasisStrong => 0xbf956aff,
            HighlightKind::Enum => 0x6eb4bfff,
            HighlightKind::Function => 0x73ade9ff,
            HighlightKind::Keyword => 0xb477cfff,
            HighlightKind::Label => 0x74ade8ff,
            HighlightKind::LinkText => 0x73ade9ff,
            HighlightKind::LinkUri => 0x6eb4bfff,
            HighlightKind::Number => 0xbf956aff,
            HighlightKind::Operator => 0x6eb4bfff,
            HighlightKind::Preproc => 0xb477cfff,
            HighlightKind::Property => 0xd07277ff,
            HighlightKind::Punctuation => 0xacb2beff,
            HighlightKind::PunctuationBracket => 0xb2b9c6ff,
            HighlightKind::PunctuationDelimiter => 0xb2b9c6ff,
            HighlightKind::PunctuationListMarker => 0xd07277ff,
            HighlightKind::PunctuationSpecial => 0xb1574bff,
            HighlightKind::String => 0xa1c181ff,
            HighlightKind::StringEscape => 0x878e98ff,
            HighlightKind::StringRegex => 0xbf956aff,
            HighlightKind::StringSpecial => 0xbf956aff,
            HighlightKind::StringSpecialSymbol => 0xbf956aff,
            HighlightKind::Tag => 0x74ade8ff,
            HighlightKind::TextLiteral => 0xa1c181ff,
            HighlightKind::Title => 0xd07277ff,
            HighlightKind::Type => 0x6eb4bfff,
            HighlightKind::Variable => 0xacb2beff,
            HighlightKind::VariableParameter => 0xd07277ff,
            HighlightKind::VariableSpecial => 0xbf956aff,
            HighlightKind::Variant => 0x73ade9ff,
        })
    }

    /// Zed's One Light syntax palette.
    pub fn one_light() -> Self {
        Self::from_palette(|kind| match kind {
            HighlightKind::Attribute => 0x5c78e2ff,
            HighlightKind::Boolean => 0xad6e25ff,
            HighlightKind::Comment => 0xa2a3a7ff,
            HighlightKind::CommentDoc => 0x7c7e86ff,
            HighlightKind::Constant => 0xc18401ff,
            HighlightKind::Constructor => 0x5c78e2ff,
            HighlightKind::Embedded => 0x242529ff,
            HighlightKind::Emphasis => 0x5c78e2ff,
            HighlightKind::EmphasisStrong => 0xad6e25ff,
            HighlightKind::Enum => 0x3882b7ff,
            HighlightKind::Function => 0x5b79e3ff,
            HighlightKind::Keyword => 0xa449abff,
            HighlightKind::Label => 0x5c78e2ff,
            HighlightKind::LinkText => 0x5b79e3ff,
            HighlightKind::LinkUri => 0x3882b7ff,
            HighlightKind::Number => 0xad6e25ff,
            HighlightKind::Operator => 0x3882b7ff,
            HighlightKind::Preproc => 0xa449abff,
            HighlightKind::Property => 0xd3604fff,
            HighlightKind::Punctuation => 0x242529ff,
            HighlightKind::PunctuationBracket => 0x4d4f52ff,
            HighlightKind::PunctuationDelimiter => 0x4d4f52ff,
            HighlightKind::PunctuationListMarker => 0xd3604fff,
            HighlightKind::PunctuationSpecial => 0xb92b46ff,
            HighlightKind::String => 0x649f57ff,
            HighlightKind::StringEscape => 0x7c7e86ff,
            HighlightKind::StringRegex => 0xad6e26ff,
            HighlightKind::StringSpecial => 0xad6e26ff,
            HighlightKind::StringSpecialSymbol => 0xad6e26ff,
            HighlightKind::Tag => 0x5c78e2ff,
            HighlightKind::TextLiteral => 0x649f57ff,
            HighlightKind::Title => 0xd3604fff,
            HighlightKind::Type => 0x3882b7ff,
            HighlightKind::Variable => 0x242529ff,
            HighlightKind::VariableParameter => 0xd3604fff,
            HighlightKind::VariableSpecial => 0xad6e25ff,
            HighlightKind::Variant => 0x5b79e3ff,
        })
    }
}

/// Resolved font families and sizes.
#[derive(Clone, Debug)]
pub struct ThemeFonts {
    /// Family for UI chrome.
    pub ui_family: SharedString,
    /// Base size for UI chrome.
    pub ui_size: Pixels,
    /// Monospace family for code.
    pub code_family: SharedString,
    /// Size for code.
    pub code_size: Pixels,
}

impl Default for ThemeFonts {
    fn default() -> Self {
        Self {
            ui_family: ".SystemUIFont".into(),
            ui_size: px(crate::settings::DEFAULT_UI_FONT_SIZE),
            code_family: "Zed Mono".into(),
            code_size: px(crate::settings::DEFAULT_CODE_FONT_SIZE),
        }
    }
}

/// The active theme, stored as a GPUI global.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Display name ("One Dark" / "One Light").
    pub name: SharedString,
    /// Light or dark.
    pub appearance: Appearance,
    /// Color tokens.
    pub colors: ThemeColors,
    /// Code colors by highlight kind.
    pub syntax: SyntaxTheme,
    /// Fonts.
    pub fonts: ThemeFonts,
}

impl Global for Theme {}

impl Theme {
    /// Builds the theme for an appearance with the given fonts.
    pub fn new(appearance: Appearance, fonts: ThemeFonts) -> Self {
        match appearance {
            Appearance::Dark => Self {
                name: "One Dark".into(),
                appearance,
                colors: ThemeColors::one_dark(),
                syntax: SyntaxTheme::one_dark(),
                fonts,
            },
            Appearance::Light => Self {
                name: "One Light".into(),
                appearance,
                colors: ThemeColors::one_light(),
                syntax: SyntaxTheme::one_light(),
                fonts,
            },
        }
    }
}

/// Access to the active theme from any GPUI context.
pub trait ActiveTheme {
    /// The active theme. Panics if the global was never installed, which
    /// [`crate::run_gpui_app`] does before opening any window.
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}
