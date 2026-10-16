//! Small styled building blocks shared by the shell views.

use gpui::{App, Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use crate::theme::ActiveTheme;

/// A borderless, Zed-style ghost button. Callers attach `on_click`.
pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, cx: &App) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .h(px(24.0))
        .px_2()
        .rounded_md()
        .text_sm()
        .text_color(colors.text)
        .cursor_pointer()
        .hover(|style| style.bg(colors.element_hover))
        .active(|style| style.bg(colors.element_selected))
        .child(label.into())
}

/// A button rendered as selected, for toggles and segmented controls.
pub fn toggle_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    let button = button(id, label, cx);
    if selected {
        button.bg(colors.element_selected)
    } else {
        button.text_color(colors.text_muted)
    }
}

/// A primary button filled with the accent color.
pub fn primary_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    let colors = &cx.theme().colors;
    let accent = colors.accent;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(28.0))
        .px_3()
        .rounded_md()
        .bg(accent)
        .text_sm()
        .text_color(colors.accent_text)
        .cursor_pointer()
        .hover(move |style| style.bg(accent.opacity(0.85)))
        .child(label.into())
}

/// A button for an action that is not available yet; it does not react to the
/// mouse.
pub fn disabled_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
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
        .rounded_md()
        .border_1()
        .border_color(colors.border_variant)
        .text_sm()
        .text_color(colors.text_placeholder)
        .child(label.into())
}

/// A keycap showing a formatted keystroke.
pub fn kbd(keys: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.background)
        .font_family(theme.fonts.code_family.clone())
        .text_xs()
        .text_color(theme.colors.text_muted)
        .child(keys.into())
}

/// A dock or sheet header row.
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
        .text_xs()
        .text_color(colors.text_muted)
        .child(title.into().to_uppercase())
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
