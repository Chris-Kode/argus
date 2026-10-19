//! Small styled building blocks shared by the shell views.
//!
//! Every component draws its corners from the [`radius`] scale (Panoptes #56
//! §4); pills (`rounded_full`) are reserved for status chips and avatars.

use std::rc::Rc;
use std::time::Duration;

use argus_diff::ChangeKind;
use argus_diff::review::CommentStatus;
use gpui::{
    Animation, AnimationExt, AnyElement, App, BoxShadow, Div, ElementId, FontWeight, Hsla, Pixels,
    RenderOnce, SharedString, Stateful, Window, div, point, prelude::*, pulsating_between, px,
};

use crate::file_tree::model::kind_letter;
use crate::icons::{IconName, icon};
use crate::theme::{ActiveTheme, ThemeColors};

/// The corner-radius scale (Panoptes #56 §4). Components use these instead
/// of GPUI's `rounded_*` helpers so every surface shares one shape language.
pub mod radius {
    use gpui::{Pixels, px};

    /// Letter badges (A/M/D), keycaps, inline code.
    pub const XS: Pixels = px(4.0);
    /// Buttons, inputs, tabs, segmented items, tree rows.
    pub const SM: Pixels = px(6.0);
    /// Control groups (breadcrumb, segmented control, split buttons), insets
    /// and swatches.
    pub const MD: Pixels = px(8.0);
    /// Cards: file, thread and run cards, sheets, popovers.
    pub const LG: Pixels = px(12.0);
}

/// Icon size inside buttons and chips.
const BUTTON_ICON_SIZE: Pixels = px(14.0);
/// Icon size inside status chips.
const CHIP_ICON_SIZE: Pixels = px(12.0);
/// UI text size of panel and section titles.
const TITLE_TEXT_SIZE: Pixels = px(12.5);
/// One loop of the Claude activity pulse.
const ACTIVITY_PULSE: Duration = Duration::from_millis(1800);

/// The shared ghost-button shell: label color `text_color`, optional
/// leading icon in the same color.
fn ghost_button(
    id: impl Into<ElementId>,
    icon_name: Option<IconName>,
    label: SharedString,
    text_color: Hsla,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .h(px(24.0))
        .px_2()
        .rounded(radius::SM)
        .text_sm()
        .text_color(text_color)
        .cursor_pointer()
        .hover(|style| style.bg(colors.element_hover))
        .active(|style| style.bg(colors.element_selected))
        .when_some(icon_name, |this, name| {
            this.child(icon(name, BUTTON_ICON_SIZE, text_color))
        })
        .child(label)
}

/// A borderless, Zed-style ghost button. Callers attach `on_click`.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, cx: &App) -> Stateful<Div> {
    let text = cx.theme().colors.text;
    ghost_button(id, None, label.into(), text, cx)
}

/// [`button`] with a leading icon.
pub fn button_with_icon(
    id: impl Into<ElementId>,
    icon_name: IconName,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let text = cx.theme().colors.text;
    ghost_button(id, Some(icon_name), label.into(), text, cx)
}

/// [`button_with_icon`] drawn in `text_muted`, for secondary chrome actions
/// (e.g. Fetch in the title bar).
pub fn muted_button_with_icon(
    id: impl Into<ElementId>,
    icon_name: IconName,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let text = cx.theme().colors.text_muted;
    ghost_button(id, Some(icon_name), label.into(), text, cx)
}

/// A ghost button for an action that is not available right now: same
/// shape as [`muted_button_with_icon`] but in `text_placeholder`, without a
/// border and without hover feedback.
pub fn disabled_ghost_button_with_icon(
    id: impl Into<ElementId>,
    icon_name: IconName,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let color = cx.theme().colors.text_placeholder;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .h(px(24.0))
        .px_2()
        .rounded(radius::SM)
        .text_sm()
        .text_color(color)
        .child(icon(icon_name, BUTTON_ICON_SIZE, color))
        .child(label.into())
}

/// [`icon_button`] for an action that is not available right now: drawn in
/// `text_placeholder`, without hover feedback. Keeps the tooltip so the
/// icon is still named.
pub fn disabled_icon_button(
    id: impl Into<ElementId>,
    icon_name: IconName,
    tooltip: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let color = cx.theme().colors.text_placeholder;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(26.0))
        .h(px(22.0))
        .rounded(radius::SM)
        .child(icon(icon_name, BUTTON_ICON_SIZE, color))
        .tooltip(text_tooltip(tooltip.into()))
}

/// A button rendered as selected, for toggles and segmented controls.
pub fn toggle_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    toggle_button_inner(id, None, label.into(), selected, cx)
}

/// [`toggle_button`] with a leading icon.
pub fn toggle_button_with_icon(
    id: impl Into<ElementId>,
    icon_name: IconName,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    toggle_button_inner(id, Some(icon_name), label.into(), selected, cx)
}

fn toggle_button_inner(
    id: impl Into<ElementId>,
    icon_name: Option<IconName>,
    label: SharedString,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    if selected {
        ghost_button(id, icon_name, label, colors.text, cx).bg(colors.element_selected)
    } else {
        ghost_button(id, icon_name, label, colors.text_muted, cx)
    }
}

/// An icon-only ghost button (26 × 22 px). The tooltip is required because
/// the icon alone does not name the action.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon_name: IconName,
    tooltip: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    icon_button_inner(id, icon_name, tooltip.into(), false, cx)
}

/// [`icon_button`] for toggles: drawn on `element_selected` when `selected`.
pub fn toggle_icon_button(
    id: impl Into<ElementId>,
    icon_name: IconName,
    tooltip: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    icon_button_inner(id, icon_name, tooltip.into(), selected, cx)
}

fn icon_button_inner(
    id: impl Into<ElementId>,
    icon_name: IconName,
    tooltip: SharedString,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    let color = if selected {
        colors.text
    } else {
        colors.text_muted
    };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(26.0))
        .h(px(22.0))
        .rounded(radius::SM)
        .cursor_pointer()
        .when(selected, |this| this.bg(colors.element_selected))
        .hover(|style| style.bg(colors.element_hover))
        .active(|style| style.bg(colors.element_selected))
        .child(icon(icon_name, BUTTON_ICON_SIZE, color))
        .tooltip(text_tooltip(tooltip))
}

/// A primary button filled with the accent color.
pub fn primary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    primary_button_inner(id, None, label.into(), cx)
}

/// [`primary_button`] with a leading icon (e.g. Accept with a check).
pub fn primary_button_with_icon(
    id: impl Into<ElementId>,
    icon_name: IconName,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    primary_button_inner(id, Some(icon_name), label.into(), cx)
}

fn primary_button_inner(
    id: impl Into<ElementId>,
    icon_name: Option<IconName>,
    label: SharedString,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    let label_color = colors.accent_text;
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap_1()
        .h(px(28.0))
        .px_3()
        .rounded(radius::SM)
        .bg(colors.accent)
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(label_color)
        .cursor_pointer()
        .hover(|style| style.opacity(0.85))
        // Pressed: nudge the button down by one pixel instead of recoloring.
        .active(|style| style.top(px(1.0)))
        .when_some(icon_name, |this, name| {
            this.child(icon(name, BUTTON_ICON_SIZE, label_color))
        })
        .child(label)
}

/// An outlined button: a ghost button with a `border` outline, for the
/// secondary decision next to a [`primary_button`] (e.g. Reject).
pub fn outline_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    ghost_button(id, None, label.into(), colors.text, cx)
        .border_1()
        .border_color(colors.border)
}

/// The Accept button of a proposal, identical in a thread's proposal inset
/// and in the proposal tab: a [`primary_button`] with a check.
pub fn accept_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    primary_button_with_icon(id, IconName::Check, label, cx).h(px(24.0))
}

/// The Reject button of a proposal: an [`outline_button`].
pub fn reject_button(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    outline_button(id, "Reject", cx)
}

/// The Reply button of a proposal: a ghost button with a speech bubble.
pub fn reply_button(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    button_with_icon(id, IconName::ChatText, "Reply", cx)
}

/// A button for an action that is not available yet; it does not react to the
/// mouse.
pub fn disabled_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    disabled_button_inner(id, None, label.into(), cx)
}

/// [`disabled_button`] with a leading icon.
pub fn disabled_button_with_icon(
    id: impl Into<ElementId>,
    icon_name: IconName,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    disabled_button_inner(id, Some(icon_name), label.into(), cx)
}

fn disabled_button_inner(
    id: impl Into<ElementId>,
    icon_name: Option<IconName>,
    label: SharedString,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    let color = colors.text_placeholder;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .h(px(24.0))
        .px_2()
        .rounded(radius::SM)
        .border_1()
        .border_color(colors.border_variant)
        .text_sm()
        .text_color(color)
        .when_some(icon_name, |this, name| {
            this.child(icon(name, BUTTON_ICON_SIZE, color))
        })
        .child(label)
}

/// A segmented control: a group of toggles of which one is selected. Item
/// `i` gets the element id `(id, i)`; [`Segmented::on_select`] receives the
/// clicked index.
pub fn segmented(
    id: impl Into<SharedString>,
    items: impl IntoIterator<Item = impl Into<SharedString>>,
    selected: usize,
    cx: &App,
) -> Segmented {
    let _ = cx;
    Segmented {
        id: id.into(),
        items: items.into_iter().map(Into::into).collect(),
        selected,
        on_select: None,
    }
}

type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// A control group built by [`segmented`].
#[derive(IntoElement)]
pub struct Segmented {
    id: SharedString,
    items: Vec<SharedString>,
    selected: usize,
    on_select: Option<SelectHandler>,
}

impl Segmented {
    /// Called with the index of the clicked item.
    pub fn on_select(mut self, handler: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Segmented {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let items = self.items.into_iter().enumerate().map(|(index, label)| {
            let item = toggle_button((self.id.clone(), index), label, index == self.selected, cx)
                .h(px(22.0));
            match self.on_select.clone() {
                Some(handler) => item
                    .on_click(move |_, window, cx| handler(index, window, cx))
                    .into_any_element(),
                None => item.into_any_element(),
            }
        });
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_0p5()
            .p(px(2.0))
            .rounded(radius::MD)
            .bg(colors.surface)
            .border_1()
            .border_color(colors.border_variant)
            .children(items.collect::<Vec<AnyElement>>())
    }
}

/// A keycap showing a formatted keystroke.
pub fn kbd(keys: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex_none()
        .px_1()
        .rounded(radius::XS)
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.background)
        .font_family(theme.fonts.code_family.clone())
        .text_xs()
        .text_color(theme.colors.text_muted)
        .child(keys.into())
}

/// A dock or sheet header row with a sentence-case title.
pub fn panel_header(title: impl Into<SharedString>, cx: &App) -> Div {
    let colors = &cx.theme().colors;
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(px(32.0))
        .px_3()
        .border_b_1()
        .border_color(colors.border_variant)
        .text_size(TITLE_TEXT_SIZE)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors.text)
        .child(title.into())
}

/// Centered muted text for empty panels.
pub fn placeholder(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .p_4()
        .child(
            div()
                .w_full()
                .text_sm()
                .text_center()
                .text_color(cx.theme().colors.text_placeholder)
                .child(text.into()),
        )
}

/// The label and color of a status (spec §7.2, Panoptes #56 §3): Claude at
/// work is `claude`, a proposal waiting for the user is `accent`, and states
/// that need nobody's attention stay neutral.
pub fn status_style(status: CommentStatus, colors: &ThemeColors) -> (&'static str, Hsla) {
    match status {
        CommentStatus::Open => ("Open", colors.text_muted),
        CommentStatus::Running => ("Running", colors.claude),
        CommentStatus::Proposed => ("Proposed", colors.accent),
        CommentStatus::Resolved => ("Resolved", colors.text_muted),
        CommentStatus::Dismissed => ("Dismissed", colors.text_placeholder),
    }
}

/// The pill shell shared by status chips.
fn pill() -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_0p5()
        .px_1p5()
        .rounded_full()
        .border_1()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
}

/// A comment status chip, styled per Panoptes #56 §3: Open is an outline,
/// Running is gold, Proposed is teal, Resolved is an outline with a check and
/// Dismissed is a struck-through faint label without a border.
pub fn status_chip(status: CommentStatus, colors: &ThemeColors) -> Div {
    let (label, color) = status_style(status, colors);
    match status {
        CommentStatus::Open => pill().border_color(colors.border).text_color(color),
        CommentStatus::Running => pill()
            .bg(colors.claude_soft)
            .border_color(colors.claude_border)
            .text_color(colors.claude_text),
        CommentStatus::Proposed => pill()
            .bg(colors.accent_soft)
            .border_color(colors.accent_border)
            .text_color(colors.accent),
        CommentStatus::Resolved => pill()
            .border_color(colors.border)
            .text_color(color)
            .child(icon(IconName::Check, CHIP_ICON_SIZE, color)),
        CommentStatus::Dismissed => pill()
            .border_color(gpui::transparent_black())
            .text_color(color)
            .line_through(),
    }
    .child(label)
}

/// A small rounded label tinted with `color`, for chips that are not a
/// comment status (e.g. "Outdated" in `warning`).
pub fn chip(label: &'static str, color: Hsla) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.6))
        .bg(color.opacity(0.15))
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(label)
}

/// The two sizes of a [`letter_badge`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BadgeSize {
    /// 15 px, for file tree rows.
    #[default]
    Small,
    /// 17 px, for file card headers.
    Large,
}

/// The `A` / `M` / `D` / `R` / `T` badge of a changed file: a square on a
/// fill tinted at 16% of its color.
pub fn letter_badge(kind: ChangeKind, size: BadgeSize, cx: &App) -> Div {
    let theme = cx.theme();
    let colors = &theme.colors;
    let color = match kind {
        ChangeKind::Added => colors.diff_added_text,
        ChangeKind::Modified => colors.file_modified,
        ChangeKind::Deleted => colors.diff_removed_text,
        ChangeKind::Renamed => colors.accent,
        ChangeKind::ModeChanged | ChangeKind::TypeChanged => colors.text_muted,
    };
    let (side, text_size) = match size {
        BadgeSize::Small => (px(15.0), px(10.0)),
        BadgeSize::Large => (px(17.0), px(11.0)),
    };
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(side)
        .rounded(radius::XS)
        .bg(color.opacity(0.16))
        .font_family(theme.fonts.code_family.clone())
        .text_size(text_size)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(kind_letter(kind))
}

/// The "Claude is working" indicator: the one-color eye mark in `claude`,
/// pulsing between full and 40% opacity on a 1.8 s loop. Static when
/// [`App::reduce_motion`] is set.
///
/// The pulse is phase-locked to a clock shared by the whole app, so several
/// indicators on screen breathe together.
pub fn claude_activity(size: Pixels, cx: &App) -> AnyElement {
    let mark = icon(IconName::ArgusEyeMono, size, cx.theme().colors.claude);
    if cx.reduce_motion() {
        return mark.into_any_element();
    }
    mark.with_animation(
        "claude-activity",
        Animation::new(ACTIVITY_PULSE)
            .repeat_synced()
            .with_easing(pulsating_between(0.4, 1.0)),
        |mark, opacity| mark.opacity(opacity),
    )
    .into_any_element()
}

/// A settings-section title with a trailing control (e.g. "Reset to
/// defaults").
pub fn section_header(title: impl Into<SharedString>, trailing: impl IntoElement, cx: &App) -> Div {
    let colors = &cx.theme().colors;
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .mt_2()
        .mb_1()
        .pb_1()
        .border_b_1()
        .border_color(colors.border_variant)
        .child(
            div()
                .text_size(TITLE_TEXT_SIZE)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text)
                .child(title.into()),
        )
        .child(trailing)
}

/// A plain-text tooltip, for [`gpui::StatefulInteractiveElement::tooltip`]:
/// `.tooltip(text_tooltip("Why this is disabled"))`.
pub fn text_tooltip(
    text: impl Into<SharedString>,
) -> impl Fn(&mut gpui::Window, &mut App) -> gpui::AnyView + 'static {
    let text = text.into();
    move |_, cx| {
        let text = text.clone();
        cx.new(|_| TextTooltip(text)).into()
    }
}

struct TextTooltip(SharedString);

impl gpui::Render for TextTooltip {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let colors = &cx.theme().colors;
        div()
            .max_w(px(320.0))
            .px_2()
            .py_1()
            .rounded(radius::LG)
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface)
            .shadow(vec![BoxShadow {
                color: colors.shadow,
                offset: point(px(0.0), px(4.0)),
                blur_radius: px(12.0),
                spread_radius: px(0.0),
                inset: false,
            }])
            .text_xs()
            .text_color(colors.text)
            .child(self.0.clone())
    }
}
