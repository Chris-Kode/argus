//! Icons and the Argus eye mark.
//!
//! The icon family is Phosphor Icons, regular weight (MIT, see
//! `assets/icons/LICENSE`), vendored unmodified. GPUI renders an SVG as an
//! alpha mask tinted with the element's text color, so one file serves every
//! theme. Sizes in use are 12, 13 and 14 px.

use gpui::{
    App, Hsla, IntoElement, Pixels, Styled, Svg, Transformation, div, prelude::*, size, svg,
};

use crate::assets::{EYE_IRIS_PATH, EYE_OUTLINE_PATH, EYE_PUPIL_PATH};
use crate::theme::{ActiveTheme, EYE_PUPIL};

macro_rules! icon_names {
    ($($variant:ident => $file:literal,)+) => {
        /// A compiled-in icon; one variant per file under `assets/icons/`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum IconName {
            $(
                #[doc = concat!("`", $file, "`")]
                $variant,
            )+
        }

        impl IconName {
            /// Every icon, in declaration order.
            pub const ALL: &'static [IconName] = &[$(IconName::$variant,)+];

            /// The asset path GPUI resolves through [`crate::Assets`].
            pub fn path(self) -> &'static str {
                match self {
                    $(IconName::$variant => concat!("icons/", $file),)+
                }
            }

            /// The embedded SVG bytes.
            pub(crate) fn bytes(self) -> &'static [u8] {
                match self {
                    $(IconName::$variant => include_bytes!(concat!("../assets/icons/", $file)),)+
                }
            }
        }
    };
}

icon_names! {
    ArgusEyeMono => "argus-eye-mono.svg",
    ArrowLineDown => "arrow-line-down.svg",
    ArrowLineUp => "arrow-line-up.svg",
    ArrowSquareOut => "arrow-square-out.svg",
    ArrowsLeftRight => "arrows-left-right.svg",
    CaretDown => "caret-down.svg",
    CaretRight => "caret-right.svg",
    ChatText => "chat-text.svg",
    Check => "check.svg",
    DotsThree => "dots-three.svg",
    Files => "files.svg",
    FolderSimple => "folder-simple.svg",
    GearSix => "gear-six.svg",
    GitBranch => "git-branch.svg",
    GitPullRequest => "git-pull-request.svg",
    Keyboard => "keyboard.svg",
    MagnifyingGlass => "magnifying-glass.svg",
    Sidebar => "sidebar.svg",
    SidebarSimple => "sidebar-simple.svg",
    SlidersHorizontal => "sliders-horizontal.svg",
    Sparkle => "sparkle.svg",
    Warning => "warning.svg",
    X => "x.svg",
}

/// A square icon of `size`, tinted with `color`.
pub fn icon(name: IconName, size: Pixels, color: Hsla) -> Svg {
    svg()
        .path(name.path())
        .size(size)
        .flex_none()
        .text_color(color)
}

/// [`icon`] mirrored horizontally. Phosphor has no right-dock sidebar, so the
/// right dock toggle draws `IconName::SidebarSimple` this way.
pub fn icon_mirrored(name: IconName, icon_size: Pixels, color: Hsla) -> Svg {
    icon(name, icon_size, color).with_transformation(Transformation::scale(size(-1.0, 1.0)))
}

/// The three-color Argus eye mark: outline in `accent`, iris in `claude`
/// and pupil in [`EYE_PUPIL`]. The layers are stacked `svg()` elements
/// because one SVG can only take one tint.
pub fn eye_mark(size: Pixels, cx: &App) -> impl IntoElement {
    let colors = &cx.theme().colors;
    let layer = |path: &'static str, color: Hsla| {
        svg()
            .path(path)
            .absolute()
            .top_0()
            .left_0()
            .size(size)
            .text_color(color)
    };
    div()
        .relative()
        .flex_none()
        .size(size)
        .child(layer(EYE_OUTLINE_PATH, colors.accent))
        .child(layer(EYE_IRIS_PATH, colors.claude))
        .child(layer(EYE_PUPIL_PATH, EYE_PUPIL))
}
