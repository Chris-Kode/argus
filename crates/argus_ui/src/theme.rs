//! Argus themes: the Panoptes Night / Day palettes (default) and Zed's One
//! Dark / One Light (fallback), plus the UI and code fonts.
//!
//! Color carries meaning only: `accent` (teal in Panoptes) is the user's move,
//! `claude` (gold) is Claude at work, and everything else stays quiet.
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
    /// Always the dark variant of the family.
    Dark,
    /// Always the light variant of the family.
    Light,
}

/// The palette family, persisted in the settings next to [`ThemeMode`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeFamily {
    /// Argus' own palette: Panoptes Night / Panoptes Day.
    #[default]
    Panoptes,
    /// Zed's One Dark / One Light, kept as a fallback.
    One,
}

impl ThemeFamily {
    /// Every family, in the order the settings sheet lists them.
    pub const ALL: [ThemeFamily; 2] = [ThemeFamily::Panoptes, ThemeFamily::One];

    /// Human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            ThemeFamily::Panoptes => "Panoptes",
            ThemeFamily::One => "One",
        }
    }
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
    /// Panoptes Night or One Dark.
    Dark,
    /// Panoptes Day or One Light.
    Light,
}

/// The fixed indigo of the eye mark's pupil, identical in every theme. A
/// brand constant rather than a token: `#3E4AA8`, in HSL.
pub const EYE_PUPIL: Hsla = Hsla {
    // hue 233.21°, saturation 106/230, lightness 230/510.
    h: 0.647_799,
    s: 0.460_870,
    l: 0.450_980,
    a: 1.0,
};

/// Semantic color tokens. Views use these names, never raw colors.
#[derive(Clone, Debug)]
pub struct ThemeColors {
    /// Editor and diff background.
    pub background: Hsla,
    /// Panels, docks, tab bars and file headers.
    pub surface: Hsla,
    /// Cards and floating surfaces: threads, sheets, popovers, dialogs.
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
    /// Secondary text and quiet statuses (open, resolved).
    pub text_muted: Hsla,
    /// Faint text, line numbers, placeholders and disabled text.
    pub text_placeholder: Hsla,
    /// The user's move: focus, selection, proposals waiting for review,
    /// primary buttons.
    pub accent: Hsla,
    /// Text drawn on top of an [`ThemeColors::accent`] fill.
    pub accent_text: Hsla,
    /// Tinted fill behind accent content (chips, selected rows).
    pub accent_soft: Hsla,
    /// Border of accent chips and cards; the hunk rail.
    pub accent_border: Hsla,
    /// Claude at work: fills, icons and rails of running runs.
    pub claude: Hsla,
    /// Claude-colored text.
    pub claude_text: Hsla,
    /// Background of Claude replies and running runs.
    pub claude_soft: Hsla,
    /// Border of running run cards.
    pub claude_border: Hsla,
    /// Hovered interactive element.
    pub element_hover: Hsla,
    /// Selected or pressed interactive element.
    pub element_selected: Hsla,
    /// Backdrop dimming the window behind a sheet.
    pub overlay: Hsla,
    /// Card and popover shadow (tinted, never pure black in Panoptes).
    pub shadow: Hsla,
    /// Background of a hunk header row.
    pub hunk_header_background: Hsla,
    /// Background of an added diff line.
    pub diff_added_background: Hsla,
    /// Background of the changed words inside an added line.
    pub diff_added_word: Hsla,
    /// Foreground for "added" markers (+ counts, `A` badge, gutter).
    pub diff_added_text: Hsla,
    /// Line number in an added row.
    pub diff_added_number: Hsla,
    /// Background of a removed diff line.
    pub diff_removed_background: Hsla,
    /// Background of the changed words inside a removed line.
    pub diff_removed_word: Hsla,
    /// Foreground for "removed" markers (− counts, `D` badge, gutter).
    pub diff_removed_text: Hsla,
    /// Line number in a removed row.
    pub diff_removed_number: Hsla,
    /// The `M` (modified file) badge.
    pub file_modified: Hsla,
    /// Warnings (outdated comments, conflicts, notices).
    pub warning: Hsla,
    /// Errors (failed runs, failed operations).
    pub error: Hsla,
}

fn color(hex: u32) -> Hsla {
    let rgba: Rgba = rgba(hex);
    rgba.into()
}

impl ThemeColors {
    /// The Panoptes Night palette (spec #56 §2.1).
    pub fn panoptes_night() -> Self {
        Self {
            background: color(0x0e1417ff),
            surface: color(0x121a1eff),
            elevated_surface: color(0x17222aff),
            title_bar: color(0x0a0f12ff),
            status_bar: color(0x0a0f12ff),
            border: color(0x22313aff),
            border_variant: color(0x1a262dff),
            text: color(0xdce6e8ff),
            text_muted: color(0x8fa3aaff),
            text_placeholder: color(0x71868eff),
            accent: color(0x34c3b4ff),
            accent_text: color(0x06221fff),
            accent_soft: color(0x34c3b422),
            accent_border: color(0x34c3b473),
            claude: color(0xe3b04bff),
            claude_text: color(0xe3b04bff),
            claude_soft: color(0xe3b04b12),
            claude_border: color(0xe3b04b55),
            element_hover: color(0x17232aff),
            element_selected: color(0x1c2e33ff),
            overlay: color(0x050b0dcc),
            shadow: color(0x030a0c8c),
            hunk_header_background: color(0x101a1eff),
            diff_added_background: color(0x0f2620ff),
            diff_added_word: color(0x3fb98a4d),
            diff_added_text: color(0x5bcb98ff),
            diff_added_number: color(0x6fb894ff),
            diff_removed_background: color(0x2a1519ff),
            diff_removed_word: color(0xe0727a4d),
            diff_removed_text: color(0xee8a8fff),
            diff_removed_number: color(0xd08a8fff),
            file_modified: color(0xe58f7aff),
            warning: color(0xe8894aff),
            error: color(0xee8a8fff),
        }
    }

    /// The Panoptes Day palette (spec #56 §2.1).
    pub fn panoptes_day() -> Self {
        Self {
            background: color(0xf7fafaff),
            surface: color(0xeff4f4ff),
            elevated_surface: color(0xfcfefeff),
            title_bar: color(0xe7ededff),
            status_bar: color(0xe7ededff),
            border: color(0xd1dcddff),
            border_variant: color(0xe1e9eaff),
            text: color(0x172225ff),
            text_muted: color(0x4e5f64ff),
            text_placeholder: color(0x617176ff),
            accent: color(0x0e7f74ff),
            accent_text: color(0xf7fafaff),
            accent_soft: color(0x0e7f741f),
            accent_border: color(0x0e7f7473),
            claude: color(0xc99526ff),
            claude_text: color(0x8a5e0cff),
            claude_soft: color(0xc995261a),
            claude_border: color(0xc9952666),
            element_hover: color(0xe7ededff),
            element_selected: color(0xd7ece9ff),
            overlay: color(0x0f1f2340),
            shadow: color(0x142d321a),
            hunk_header_background: color(0xf3f7f7ff),
            diff_added_background: color(0xe3f3ecff),
            diff_added_word: color(0x2e9a6a38),
            diff_added_text: color(0x1f7f55ff),
            diff_added_number: color(0x2f7d58ff),
            diff_removed_background: color(0xfce8e8ff),
            diff_removed_word: color(0xd2505833),
            diff_removed_text: color(0xb8404aff),
            diff_removed_number: color(0xa84850ff),
            file_modified: color(0xb9532fff),
            warning: color(0xa9561aff),
            error: color(0xb8404aff),
        }
    }

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
            accent_soft: color(0x74ade822),
            accent_border: color(0x74ade873),
            claude: color(0xdec184ff),
            claude_text: color(0xdec184ff),
            claude_soft: color(0xdec18412),
            claude_border: color(0xdec18455),
            element_hover: color(0x363c46ff),
            element_selected: color(0x454a56ff),
            overlay: color(0x0000008c),
            shadow: color(0x0000008c),
            hunk_header_background: color(0x2f343eff),
            diff_added_background: color(0x222e1dff),
            diff_added_word: color(0xa7c08840),
            diff_added_text: color(0xa7c088ff),
            diff_added_number: color(0x878a98ff),
            diff_removed_background: color(0x301b1bff),
            diff_removed_word: color(0xd0727740),
            diff_removed_text: color(0xd07277ff),
            diff_removed_number: color(0x878a98ff),
            file_modified: color(0xdec184ff),
            warning: color(0xdec184ff),
            error: color(0xd07277ff),
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
            accent_soft: color(0x5c78e222),
            accent_border: color(0x5c78e273),
            claude: color(0xa48819ff),
            claude_text: color(0xa48819ff),
            claude_soft: color(0xa4881912),
            claude_border: color(0xa4881955),
            element_hover: color(0xdfdfe0ff),
            element_selected: color(0xc9c9caff),
            overlay: color(0x00000040),
            shadow: color(0x00000026),
            hunk_header_background: color(0xebebecff),
            diff_added_background: color(0xdfeadbff),
            diff_added_word: color(0x669f5940),
            diff_added_text: color(0x669f59ff),
            diff_added_number: color(0x7e8086ff),
            diff_removed_background: color(0xfbdfd9ff),
            diff_removed_word: color(0xd3615140),
            diff_removed_text: color(0xd36151ff),
            diff_removed_number: color(0x7e8086ff),
            file_modified: color(0xa48819ff),
            warning: color(0xa48819ff),
            error: color(0xd36151ff),
        }
    }
}

/// Syntax colors by [`HighlightKind`]: the Panoptes Night / Day palettes and
/// Zed's One Dark / One Light `syntax` styles.
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
                font_style: matches!(
                    kind,
                    HighlightKind::Emphasis | HighlightKind::LinkText | HighlightKind::Comment
                )
                .then_some(FontStyle::Italic),
                ..HighlightStyle::default()
            }),
        }
    }

    /// The Panoptes Night syntax palette (spec #56 §2.2).
    pub fn panoptes_night() -> Self {
        use HighlightKind::*;
        Self::from_palette(|kind| match kind {
            Keyword | Preproc => 0xa794f2ff,
            Function | Constructor | Variant | LinkText | Label | Tag | Emphasis => 0x5cc8e0ff,
            Type | Enum | LinkUri => 0x46c2b0ff,
            String | TextLiteral => 0x9ccb7eff,
            Number | Boolean | Constant | StringRegex | StringSpecial | StringSpecialSymbol
            | VariableSpecial | EmphasisStrong => 0xf2a271ff,
            Property
            | VariableParameter
            | Attribute
            | Title
            | PunctuationListMarker
            | PunctuationSpecial => 0xe58f7aff,
            Operator => 0x8fc4cfff,
            Comment => 0x71868eff,
            CommentDoc | StringEscape | Punctuation => 0x8fa3aaff,
            PunctuationBracket | PunctuationDelimiter => 0xa7b6bbff,
            Variable | Embedded => 0xdce6e8ff,
        })
    }

    /// The Panoptes Day syntax palette (spec #56 §2.2).
    pub fn panoptes_day() -> Self {
        use HighlightKind::*;
        Self::from_palette(|kind| match kind {
            Keyword | Preproc => 0x6a4fc9ff,
            Function | Constructor | Variant | LinkText | Label | Tag | Emphasis => 0x0f7ea0ff,
            Type | Enum | LinkUri => 0x0e7f74ff,
            String | TextLiteral => 0x4f8a2eff,
            Number | Boolean | Constant | StringRegex | StringSpecial | StringSpecialSymbol
            | VariableSpecial | EmphasisStrong => 0xa0620aff,
            Property
            | VariableParameter
            | Attribute
            | Title
            | PunctuationListMarker
            | PunctuationSpecial => 0xb9532fff,
            Operator => 0x2f6f80ff,
            Comment => 0x617176ff,
            CommentDoc | StringEscape | Punctuation | PunctuationBracket | PunctuationDelimiter => {
                0x4e5f64ff
            }
            Variable | Embedded => 0x172225ff,
        })
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
            ui_family: crate::fonts::BUNDLED_UI_FAMILY.into(),
            ui_size: px(crate::settings::DEFAULT_UI_FONT_SIZE),
            code_family: crate::fonts::BUNDLED_CODE_FAMILY.into(),
            code_size: px(crate::settings::DEFAULT_CODE_FONT_SIZE),
        }
    }
}

/// The active theme, stored as a GPUI global.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Display name ("Panoptes Night", "Panoptes Day", "One Dark" or "One
    /// Light").
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
    /// Builds the theme for a family and appearance with the given fonts:
    /// Panoptes Night, Panoptes Day, One Dark or One Light.
    pub fn new(family: ThemeFamily, appearance: Appearance, fonts: ThemeFonts) -> Self {
        let (name, colors, syntax) = match (family, appearance) {
            (ThemeFamily::Panoptes, Appearance::Dark) => (
                "Panoptes Night",
                ThemeColors::panoptes_night(),
                SyntaxTheme::panoptes_night(),
            ),
            (ThemeFamily::Panoptes, Appearance::Light) => (
                "Panoptes Day",
                ThemeColors::panoptes_day(),
                SyntaxTheme::panoptes_day(),
            ),
            (ThemeFamily::One, Appearance::Dark) => {
                ("One Dark", ThemeColors::one_dark(), SyntaxTheme::one_dark())
            }
            (ThemeFamily::One, Appearance::Light) => (
                "One Light",
                ThemeColors::one_light(),
                SyntaxTheme::one_light(),
            ),
        };
        Self {
            name: name.into(),
            appearance,
            colors,
            syntax,
            fonts,
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
