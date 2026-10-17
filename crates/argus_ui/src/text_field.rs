//! A single-line text field for forms such as the settings sheet.
//!
//! GPUI has no text-input element yet, so, like the branch picker and the
//! file tree filter, this appends typed characters at the end and supports
//! backspace, clear (`alt`/`secondary`-backspace) and paste. Enter emits
//! [`TextFieldEvent::Submitted`]; the owner validates and applies the text.

use gpui::{
    Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, SharedString, Window, div,
    prelude::*, px,
};

use crate::keymap::TEXT_INPUT_CONTEXT;
use crate::theme::ActiveTheme;

/// What a [`TextField`] tells its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextFieldEvent {
    /// The text changed.
    Edited,
    /// The user pressed Enter.
    Submitted,
}

/// A focusable one-line text field.
pub struct TextField {
    focus_handle: FocusHandle,
    text: String,
    placeholder: SharedString,
    invalid: bool,
}

impl EventEmitter<TextFieldEvent> for TextField {}

impl TextField {
    /// An empty field showing `placeholder`.
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            text: String::new(),
            placeholder: placeholder.into(),
            invalid: false,
        }
    }

    /// The current text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replaces the text without emitting [`TextFieldEvent::Edited`].
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        if text != self.text {
            self.text = text;
            cx.notify();
        }
    }

    /// Replaces the placeholder.
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.into();
        if placeholder != self.placeholder {
            self.placeholder = placeholder;
            cx.notify();
        }
    }

    /// Draws the border in the error color.
    pub fn set_invalid(&mut self, invalid: bool, cx: &mut Context<Self>) {
        if invalid != self.invalid {
            self.invalid = invalid;
            cx.notify();
        }
    }

    fn edit(&mut self, text: String, cx: &mut Context<Self>) {
        if text != self.text {
            self.text = text;
            cx.emit(TextFieldEvent::Edited);
            cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        match keystroke.key.as_str() {
            "enter" => cx.emit(TextFieldEvent::Submitted),
            "backspace" if modifiers.secondary() || modifiers.alt => self.edit(String::new(), cx),
            "backspace" => {
                let mut text = self.text.clone();
                text.pop();
                self.edit(text, cx);
            }
            "v" if modifiers.secondary() => {
                if let Some(pasted) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let line = pasted.lines().next().unwrap_or_default().trim();
                    let text = format!("{}{line}", self.text);
                    self.edit(text, cx);
                }
            }
            _ => {
                if modifiers.control || modifiers.platform || modifiers.function {
                    return;
                }
                let Some(typed) = keystroke.key_char.as_deref() else {
                    return;
                };
                if typed.chars().any(char::is_control) {
                    return;
                }
                let text = format!("{}{typed}", self.text);
                self.edit(text, cx);
            }
        }
        cx.stop_propagation();
    }
}

impl Focusable for TextField {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let focused = self.focus_handle.is_focused(window);
        let accent = colors.accent;
        let caret = move || div().flex_none().w(px(1.5)).h(px(14.0)).bg(accent);
        let empty = self.text.is_empty();
        let text: SharedString = if empty {
            self.placeholder.clone()
        } else {
            self.text.clone().into()
        };
        div()
            .id("text-field")
            .key_context(TEXT_INPUT_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap_0p5()
            .h(px(24.0))
            .px_2()
            .rounded_md()
            .border_1()
            .border_color(if self.invalid {
                colors.error
            } else if focused {
                colors.accent
            } else {
                colors.border_variant
            })
            .bg(colors.background)
            .cursor_text()
            .text_sm()
            .overflow_hidden()
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
            .when(focused && empty, |field| field.child(caret()))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(if empty {
                        colors.text_placeholder
                    } else {
                        colors.text
                    })
                    .child(text),
            )
            .when(focused && !empty, |field| field.child(caret()))
    }
}
