//! Argus themes: two UI styles, each with a dark and a light palette, plus
//! the UI and code fonts.
//!
//! The [`UiStyle`] picks the family: Zed (One Dark / One Light, rounded
//! chrome) or VS Code (Dark Modern / Light Modern, square chrome with an
//! activity bar). The [`ThemeMode`] picks the variant within it.
//!
//! Color carries meaning only: `accent` (blue) is the user's move,
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

/// The look of the window chrome, persisted in the settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiStyle {
    /// Zed: One Dark / One Light, rounded controls, Files | Comments tabs
    /// over the left dock.
    #[default]
    Zed,
    /// Visual Studio Code: Dark Modern / Light Modern, square controls, an
    /// activity bar and a centered title.
    VsCode,
}

impl UiStyle {
    /// Every style, in the order the settings sheet lists them.
    pub const ALL: [UiStyle; 2] = [UiStyle::Zed, UiStyle::VsCode];

    /// Human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            UiStyle::Zed => "Zed",
            UiStyle::VsCode => "VS Code",
        }
    }

    /// The corner radii of the style.
    pub fn radius(self) -> ThemeRadius {
        match self {
            UiStyle::Zed => ThemeRadius {
                xs: px(4.0),
                sm: px(6.0),
                md: px(8.0),
                lg: px(12.0),
            },
            UiStyle::VsCode => ThemeRadius {
                xs: px(2.0),
                sm: px(3.0),
                md: px(4.0),
                lg: px(6.0),
            },
        }
    }
}

/// Corner radii, from badges up to cards. Read through
/// [`crate::components::radius`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThemeRadius {
    /// Letter badges (A/M/D), keycaps, inline code.
    pub xs: Pixels,
    /// Buttons, inputs, tabs, segmented items, tree rows.
    pub sm: Pixels,
    /// Control groups (breadcrumb, segmented control, split buttons), insets
    /// and swatches.
    pub md: Pixels,
    /// Cards: file, thread and run cards, sheets, popovers.
    pub lg: Pixels,
}

/// The user's theme choice, persisted in the settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Follow the operating system appearance.
    #[default]
    System,
    /// Always the dark variant of the style.
    Dark,
    /// Always the light variant of the style.
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
    /// One Dark or Dark Modern.
    Dark,
    /// One Light or Light Modern.
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
    /// Card and popover shadow.
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

    /// VS Code's Dark Modern palette.
    pub fn vscode_dark() -> Self {
        Self {
            background: color(0x1f1f1fff),
            surface: color(0x181818ff),
            elevated_surface: color(0x252526ff),
            title_bar: color(0x181818ff),
            status_bar: color(0x181818ff),
            border: color(0x2b2b2bff),
            border_variant: color(0x2b2b2bff),
            text: color(0xccccccff),
            text_muted: color(0x9d9d9dff),
            text_placeholder: color(0x6e7681ff),
            accent: color(0x0078d4ff),
            accent_text: color(0xffffffff),
            accent_soft: color(0x0078d433),
            accent_border: color(0x0078d4aa),
            claude: color(0xe2c08dff),
            claude_text: color(0xe2c08dff),
            claude_soft: color(0xe2c08d12),
            claude_border: color(0xe2c08d55),
            element_hover: color(0x2a2d2eff),
            element_selected: color(0x37373dff),
            overlay: color(0x00000099),
            shadow: color(0x0000005c),
            hunk_header_background: color(0x252526ff),
            diff_added_background: color(0x2a3424ff),
            diff_added_word: color(0x9ccc2c40),
            diff_added_text: color(0x81b88bff),
            diff_added_number: color(0x6e7681ff),
            diff_removed_background: color(0x3c1e1eff),
            diff_removed_word: color(0xff000040),
            diff_removed_text: color(0xf85149ff),
            diff_removed_number: color(0x6e7681ff),
            file_modified: color(0xe2c08dff),
            warning: color(0xcca700ff),
            error: color(0xf85149ff),
        }
    }

    /// VS Code's Light Modern palette.
    pub fn vscode_light() -> Self {
        Self {
            background: color(0xffffffff),
            surface: color(0xf8f8f8ff),
            elevated_surface: color(0xf8f8f8ff),
            title_bar: color(0xf8f8f8ff),
            status_bar: color(0xf8f8f8ff),
            border: color(0xe5e5e5ff),
            border_variant: color(0xe5e5e5ff),
            text: color(0x3b3b3bff),
            text_muted: color(0x616161ff),
            text_placeholder: color(0x8b8b8bff),
            accent: color(0x005fb8ff),
            accent_text: color(0xffffffff),
            accent_soft: color(0x005fb81f),
            accent_border: color(0x005fb873),
            claude: color(0x895503ff),
            claude_text: color(0x895503ff),
            claude_soft: color(0x89550312),
            claude_border: color(0x89550355),
            element_hover: color(0xf2f2f2ff),
            element_selected: color(0xe4e6f1ff),
            overlay: color(0x00000040),
            shadow: color(0x00000029),
            hunk_header_background: color(0xf3f3f3ff),
            diff_added_background: color(0xebf1ddff),
            diff_added_word: color(0x9ccc2c66),
            diff_added_text: color(0x587c0cff),
            diff_added_number: color(0x8b8b8bff),
            diff_removed_background: color(0xffe0e0ff),
            diff_removed_word: color(0xff000033),
            diff_removed_text: color(0xad0707ff),
            diff_removed_number: color(0x8b8b8bff),
            file_modified: color(0x895503ff),
            warning: color(0xbf8803ff),
            error: color(0xe51400ff),
        }
    }
}

/// Syntax colors by [`HighlightKind`]: Zed's One Dark / One Light `syntax`
/// styles, or VS Code's Dark+ / Light+ token colors.
#[derive(Clone, Debug)]
pub struct SyntaxTheme {
    styles: [HighlightStyle; HighlightKind::COUNT],
}

impl SyntaxTheme {
    /// The text style of `kind`.
    pub fn style(&self, kind: HighlightKind) -> HighlightStyle {
        self.styles[kind.index()]
    }

    /// Builds the styles from a palette; `italic_comments` sets comments in
    /// italics, as Zed does and VS Code does not.
    fn from_palette(italic_comments: bool, palette: impl Fn(HighlightKind) -> u32) -> Self {
        Self {
            styles: HighlightKind::ALL.map(|kind| HighlightStyle {
                color: Some(color(palette(kind))),
                font_weight: (kind == HighlightKind::EmphasisStrong).then_some(FontWeight::BOLD),
                font_style: match kind {
                    HighlightKind::Emphasis | HighlightKind::LinkText => true,
                    HighlightKind::Comment => italic_comments,
                    _ => false,
                }
                .then_some(FontStyle::Italic),
                ..HighlightStyle::default()
            }),
        }
    }

    /// Zed's One Dark syntax palette.
    pub fn one_dark() -> Self {
        Self::from_palette(true, |kind| match kind {
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
        Self::from_palette(true, |kind| match kind {
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

    /// VS Code's Dark+ token colors (shared by Dark Modern).
    pub fn vscode_dark() -> Self {
        Self::from_palette(false, |kind| match kind {
            HighlightKind::Attribute => 0x9cdcfeff,
            HighlightKind::Boolean => 0x569cd6ff,
            HighlightKind::Comment => 0x6a9955ff,
            HighlightKind::CommentDoc => 0x6a9955ff,
            HighlightKind::Constant => 0x4fc1ffff,
            HighlightKind::Constructor => 0x4ec9b0ff,
            HighlightKind::Embedded => 0xd4d4d4ff,
            HighlightKind::Emphasis => 0xd4d4d4ff,
            HighlightKind::EmphasisStrong => 0x569cd6ff,
            HighlightKind::Enum => 0x4ec9b0ff,
            HighlightKind::Function => 0xdcdcaaff,
            HighlightKind::Keyword => 0x569cd6ff,
            HighlightKind::Label => 0xc8c8c8ff,
            HighlightKind::LinkText => 0xce9178ff,
            HighlightKind::LinkUri => 0x3794ffff,
            HighlightKind::Number => 0xb5cea8ff,
            HighlightKind::Operator => 0xd4d4d4ff,
            HighlightKind::Preproc => 0xc586c0ff,
            HighlightKind::Property => 0x9cdcfeff,
            HighlightKind::Punctuation => 0xd4d4d4ff,
            HighlightKind::PunctuationBracket => 0xd4d4d4ff,
            HighlightKind::PunctuationDelimiter => 0xd4d4d4ff,
            HighlightKind::PunctuationListMarker => 0x6796e6ff,
            HighlightKind::PunctuationSpecial => 0x569cd6ff,
            HighlightKind::String => 0xce9178ff,
            HighlightKind::StringEscape => 0xd7ba7dff,
            HighlightKind::StringRegex => 0xd16969ff,
            HighlightKind::StringSpecial => 0xd7ba7dff,
            HighlightKind::StringSpecialSymbol => 0xd7ba7dff,
            HighlightKind::Tag => 0x569cd6ff,
            HighlightKind::TextLiteral => 0xce9178ff,
            HighlightKind::Title => 0x569cd6ff,
            HighlightKind::Type => 0x4ec9b0ff,
            HighlightKind::Variable => 0x9cdcfeff,
            HighlightKind::VariableParameter => 0x9cdcfeff,
            HighlightKind::VariableSpecial => 0x569cd6ff,
            HighlightKind::Variant => 0x4fc1ffff,
        })
    }

    /// VS Code's Light+ token colors (shared by Light Modern).
    pub fn vscode_light() -> Self {
        Self::from_palette(false, |kind| match kind {
            HighlightKind::Attribute => 0xe50000ff,
            HighlightKind::Boolean => 0x0000ffff,
            HighlightKind::Comment => 0x008000ff,
            HighlightKind::CommentDoc => 0x008000ff,
            HighlightKind::Constant => 0x0070c1ff,
            HighlightKind::Constructor => 0x267f99ff,
            HighlightKind::Embedded => 0x000000ff,
            HighlightKind::Emphasis => 0x000000ff,
            HighlightKind::EmphasisStrong => 0x000080ff,
            HighlightKind::Enum => 0x267f99ff,
            HighlightKind::Function => 0x795e26ff,
            HighlightKind::Keyword => 0x0000ffff,
            HighlightKind::Label => 0x000000ff,
            HighlightKind::LinkText => 0xa31515ff,
            HighlightKind::LinkUri => 0x006ab1ff,
            HighlightKind::Number => 0x098658ff,
            HighlightKind::Operator => 0x000000ff,
            HighlightKind::Preproc => 0xaf00dbff,
            HighlightKind::Property => 0x001080ff,
            HighlightKind::Punctuation => 0x000000ff,
            HighlightKind::PunctuationBracket => 0x000000ff,
            HighlightKind::PunctuationDelimiter => 0x000000ff,
            HighlightKind::PunctuationListMarker => 0x0451a5ff,
            HighlightKind::PunctuationSpecial => 0x0000ffff,
            HighlightKind::String => 0xa31515ff,
            HighlightKind::StringEscape => 0xee0000ff,
            HighlightKind::StringRegex => 0x811f3fff,
            HighlightKind::StringSpecial => 0xee0000ff,
            HighlightKind::StringSpecialSymbol => 0xee0000ff,
            HighlightKind::Tag => 0x800000ff,
            HighlightKind::TextLiteral => 0xa31515ff,
            HighlightKind::Title => 0x800000ff,
            HighlightKind::Type => 0x267f99ff,
            HighlightKind::Variable => 0x001080ff,
            HighlightKind::VariableParameter => 0x001080ff,
            HighlightKind::VariableSpecial => 0x0000ffff,
            HighlightKind::Variant => 0x0070c1ff,
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
    /// Display name ("One Dark", "One Light", "Dark Modern" or "Light
    /// Modern").
    pub name: SharedString,
    /// Zed or VS Code chrome.
    pub style: UiStyle,
    /// Light or dark.
    pub appearance: Appearance,
    /// Corner radii of the style.
    pub radius: ThemeRadius,
    /// Color tokens.
    pub colors: ThemeColors,
    /// Code colors by highlight kind.
    pub syntax: SyntaxTheme,
    /// Fonts.
    pub fonts: ThemeFonts,
}

impl Global for Theme {}

impl Theme {
    /// Builds the theme for a style and appearance with the given fonts.
    pub fn new(style: UiStyle, appearance: Appearance, fonts: ThemeFonts) -> Self {
        let (name, colors, syntax) = match (style, appearance) {
            (UiStyle::Zed, Appearance::Dark) => {
                ("One Dark", ThemeColors::one_dark(), SyntaxTheme::one_dark())
            }
            (UiStyle::Zed, Appearance::Light) => (
                "One Light",
                ThemeColors::one_light(),
                SyntaxTheme::one_light(),
            ),
            (UiStyle::VsCode, Appearance::Dark) => (
                "Dark Modern",
                ThemeColors::vscode_dark(),
                SyntaxTheme::vscode_dark(),
            ),
            (UiStyle::VsCode, Appearance::Light) => (
                "Light Modern",
                ThemeColors::vscode_light(),
                SyntaxTheme::vscode_light(),
            ),
        };
        Self {
            name: name.into(),
            style,
            appearance,
            radius: style.radius(),
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
