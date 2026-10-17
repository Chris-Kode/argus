//! A small multi-line plain-text editor for comment bodies and replies.
//!
//! [`TextEditor`] owns the text, the selection and the IME state; it
//! implements [`EntityInputHandler`], so typing, dead keys and input
//! methods work as in any native field. The text soft-wraps to the width
//! it is given and grows with its content. Keys: the usual caret and
//! selection moves ([`crate::keymap::editor_bindings`]), `enter` for a line
//! break, `secondary-enter` to submit and `escape` to cancel; both are
//! reported as [`TextEditorEvent`]s and the owner decides what they mean.
//!
//! The editor sets the `TextInput` key context, so the single-key review
//! shortcuts (`c`, `j`, `]c`…) type letters while it is focused.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, div, fill,
    point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::keymap::{
    Backspace, COMMENT_EDITOR_CONTEXT, Copy, Cut, Delete, Dismiss, MoveDown, MoveLeft, MoveRight,
    MoveToLineEnd, MoveToLineStart, MoveUp, MoveWordLeft, MoveWordRight, Newline, Paste, SelectAll,
    SelectDown, SelectLeft, SelectRight, SelectToLineEnd, SelectToLineStart, SelectUp,
    SubmitComment, TEXT_INPUT_CONTEXT,
};
use crate::theme::ActiveTheme;

/// Width of the caret.
const CARET_WIDTH: Pixels = px(1.5);

/// What the editor reports to its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEditorEvent {
    /// The text changed.
    Edited,
    /// `secondary-enter`: the user wants to save.
    Submit,
    /// `escape`: the user wants to leave.
    Cancel,
}

/// The text as laid out by the last paint, for hit testing and caret
/// movement.
struct LastLayout {
    /// One wrapped line per `\n`-separated line of the text.
    lines: Vec<WrappedLine>,
    /// Byte offset of each line in the text.
    starts: Vec<usize>,
    /// Top of each line, relative to `bounds`.
    tops: Vec<Pixels>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
}

impl LastLayout {
    fn view(&self) -> LayoutView<'_> {
        LayoutView {
            lines: &self.lines,
            starts: &self.starts,
            tops: &self.tops,
            bounds: self.bounds,
            line_height: self.line_height,
        }
    }
}

/// Borrowed shaped lines with their positions.
#[derive(Clone, Copy)]
struct LayoutView<'a> {
    lines: &'a [WrappedLine],
    starts: &'a [usize],
    tops: &'a [Pixels],
    bounds: Bounds<Pixels>,
    line_height: Pixels,
}

impl LayoutView<'_> {
    /// The line holding byte `index`.
    fn line_of(&self, index: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= index)
            .saturating_sub(1)
    }

    /// Window position of the top-left of the caret before byte `index`.
    fn position_for_index(&self, index: usize) -> Point<Pixels> {
        let line = self.line_of(index);
        let (Some(wrapped), Some(start), Some(top)) = (
            self.lines.get(line),
            self.starts.get(line),
            self.tops.get(line),
        ) else {
            return self.bounds.origin;
        };
        let within = (index - start).min(wrapped.len());
        let local = wrapped
            .position_for_index(within, self.line_height)
            .unwrap_or_default();
        self.bounds.origin + point(local.x, *top + local.y)
    }

    /// The byte closest to window position `position`.
    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let y = position.y - self.bounds.top();
        if self.lines.is_empty() || y < Pixels::ZERO {
            return 0;
        }
        let line = self
            .tops
            .partition_point(|&top| top <= y)
            .saturating_sub(1)
            .min(self.lines.len() - 1);
        let wrapped = &self.lines[line];
        let local = point(
            (position.x - self.bounds.left()).max(Pixels::ZERO),
            (y - self.tops[line]).max(Pixels::ZERO),
        );
        let within = match wrapped.closest_index_for_position(local, self.line_height) {
            Ok(index) | Err(index) => index,
        };
        self.starts[line] + within.min(wrapped.len())
    }
}

/// A multi-line text field.
pub struct TextEditor {
    focus_handle: FocusHandle,
    text: String,
    placeholder: SharedString,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    /// Column kept while moving up and down through shorter lines.
    preferred_x: Option<Pixels>,
    selecting: bool,
    /// Focus the editor the next time it renders (callers often have no
    /// window at hand when they open it).
    focus_requested: bool,
    last_layout: Option<LastLayout>,
}

impl EventEmitter<TextEditorEvent> for TextEditor {}

impl Focusable for TextEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextEditor {
    /// An editor holding `text`, with the caret at its end.
    pub fn new(
        text: impl Into<String>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            focus_handle: cx.focus_handle(),
            text,
            placeholder: placeholder.into(),
            selected: end..end,
            reversed: false,
            marked: None,
            preferred_x: None,
            selecting: false,
            focus_requested: false,
            last_layout: None,
        }
    }

    /// The current text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the text is empty or only whitespace.
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Replaces the text and puts the caret at its end.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        let end = self.text.len();
        self.selected = end..end;
        self.marked = None;
        cx.emit(TextEditorEvent::Edited);
        cx.notify();
    }

    /// Focuses the editor at its next render.
    pub fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.focus_requested = true;
        cx.notify();
    }

    // ---- caret and selection ----------------------------------------------

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.text.len());
        self.selected = offset..offset;
        self.reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.text.len());
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.text.len())
    }

    fn previous_word(&self, offset: usize) -> usize {
        self.text
            .unicode_word_indices()
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_word(&self, offset: usize) -> usize {
        self.text
            .unicode_word_indices()
            .find_map(|(index, word)| (index + word.len() > offset).then_some(index + word.len()))
            .unwrap_or(self.text.len())
    }

    fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map_or(0, |at| at + 1)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |at| offset + at)
    }

    /// The offset one visual row above (`delta < 0`) or below the caret.
    fn vertical_target(&mut self, delta: f32) -> usize {
        let cursor = self.cursor();
        let Some(layout) = self.last_layout.as_ref().map(LastLayout::view) else {
            return if delta < 0.0 { 0 } else { self.text.len() };
        };
        let position = layout.position_for_index(cursor);
        let x = *self.preferred_x.get_or_insert(position.x);
        let y = position.y + layout.line_height * (0.5 + delta);
        if y < layout.bounds.top() {
            return 0;
        }
        if y >= layout.bounds.bottom() {
            return self.text.len();
        }
        layout.index_for_position(point(x, y))
    }

    fn left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        if self.selected.is_empty() {
            self.move_to(self.previous_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selected.start, cx);
        }
    }

    fn right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        if self.selected.is_empty() {
            self.move_to(self.next_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selected.end, cx);
        }
    }

    fn up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(-1.0);
        self.move_to(target, cx);
    }

    fn down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(1.0);
        self.move_to(target, cx);
    }

    fn word_left(&mut self, _: &MoveWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.move_to(self.previous_word(self.cursor()), cx);
    }

    fn word_right(&mut self, _: &MoveWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.move_to(self.next_word(self.cursor()), cx);
    }

    fn line_home(&mut self, _: &MoveToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.move_to(self.line_start(self.cursor()), cx);
    }

    fn line_finish(&mut self, _: &MoveToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.move_to(self.line_end(self.cursor()), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(-1.0);
        self.select_to(target, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical_target(1.0);
        self.select_to(target, cx);
    }

    fn select_line_start(&mut self, _: &SelectToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.select_to(self.line_start(self.cursor()), cx);
    }

    fn select_line_end(&mut self, _: &SelectToLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.select_to(self.line_end(self.cursor()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.selected = 0..self.text.len();
        self.reversed = false;
        cx.notify();
    }

    // ---- editing ------------------------------------------------------------

    /// Replaces `range` with `new_text` and puts the caret after it.
    fn replace(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        let range = range.start.min(self.text.len())..range.end.min(self.text.len());
        self.text.replace_range(range.clone(), new_text);
        let caret = range.start + new_text.len();
        self.selected = caret..caret;
        self.reversed = false;
        self.marked = None;
        self.preferred_x = None;
        cx.emit(TextEditorEvent::Edited);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let previous = self.previous_boundary(self.cursor());
            if previous == self.cursor() {
                return;
            }
            self.select_to(previous, cx);
        }
        self.replace(self.selected.clone(), "", cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let next = self.next_boundary(self.cursor());
            if next == self.cursor() {
                return;
            }
            self.select_to(next, cx);
        }
        self.replace(self.selected.clone(), "", cx);
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.replace(self.selected.clone(), "\n", cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            let text = self.text[self.selected.clone()].to_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            let text = self.text[self.selected.clone()].to_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.replace(self.selected.clone(), "", cx);
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n");
            self.replace(self.selected.clone(), &text, cx);
        }
    }

    fn submit(&mut self, _: &SubmitComment, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextEditorEvent::Submit);
    }

    fn cancel(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextEditorEvent::Cancel);
    }

    // ---- mouse --------------------------------------------------------------

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Clicks must not start a gutter selection or a drag underneath.
        cx.stop_propagation();
        window.focus(&self.focus_handle, cx);
        self.preferred_x = None;
        let Some(index) = self
            .last_layout
            .as_ref()
            .map(|layout| layout.view().index_for_position(event.position))
        else {
            return;
        };
        self.selecting = true;
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        if let Some(index) = self
            .last_layout
            .as_ref()
            .map(|layout| layout.view().index_for_position(event.position))
        {
            self.select_to(index, cx);
        }
    }

    // ---- UTF-16 offsets for the platform input handler --------------------

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.text.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        let mut utf8 = 0;
        for ch in self.text.chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}

impl EntityInputHandler for TextEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        self.text.get(range).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.replace(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        let range = range.start.min(self.text.len())..range.end.min(self.text.len());
        self.text.replace_range(range.clone(), new_text);
        self.marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected = new_selected_range_utf16
            .as_ref()
            .map(|selected| {
                // The new selection is relative to the inserted text.
                let selected = self.range_from_utf16(selected);
                range.start + selected.start..range.start + selected.end
            })
            .unwrap_or_else(|| {
                let caret = range.start + new_text.len();
                caret..caret
            });
        self.reversed = false;
        cx.emit(TextEditorEvent::Edited);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?.view();
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.position_for_index(range.start);
        let end = layout.position_for_index(range.end);
        Some(Bounds::from_corners(
            start,
            point(end.x.max(start.x), end.y + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.last_layout.as_ref()?.view();
        Some(self.offset_to_utf16(layout.index_for_position(point)))
    }
}

impl Render for TextEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_requested) {
            window.focus(&self.focus_handle, cx);
        }
        let colors = cx.theme().colors.clone();
        let focused = self.focus_handle.is_focused(window);
        div()
            .key_context(format!("{TEXT_INPUT_CONTEXT} {COMMENT_EDITOR_CONTEXT}").as_str())
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::line_home))
            .on_action(cx.listener(Self::line_finish))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::cancel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .w_full()
            .min_h(px(64.0))
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(if focused {
                colors.accent
            } else {
                colors.border
            })
            .bg(colors.background)
            .text_color(colors.text)
            .child(EditorElement {
                editor: cx.entity(),
            })
    }
}

/// Paints the editor's text, selection and caret, and registers the
/// platform input handler.
struct EditorElement {
    editor: Entity<TextEditor>,
}

struct EditorPrepaint {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    tops: Vec<Pixels>,
    line_height: Pixels,
    selection: Vec<PaintQuad>,
    caret: Option<PaintQuad>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The text to shape: the content, or the placeholder when empty.
fn display_runs(editor: &TextEditor, window: &Window, cx: &App) -> (SharedString, Vec<TextRun>) {
    let style = window.text_style();
    let colors = &cx.theme().colors;
    let (text, color) = if editor.text.is_empty() {
        (editor.placeholder.clone(), colors.text_placeholder)
    } else {
        (SharedString::from(editor.text.clone()), style.color)
    };
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = match editor.marked.as_ref().filter(|_| !editor.text.is_empty()) {
        Some(marked) => [
            TextRun {
                len: marked.start,
                ..run.clone()
            },
            TextRun {
                len: marked.end - marked.start,
                underline: Some(UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: text.len() - marked.end,
                ..run
            },
        ]
        .into_iter()
        .filter(|run| run.len > 0)
        .collect(),
        None => vec![run],
    };
    (text, runs)
}

/// Shapes `text` wrapped at `width`; returns the lines and their tops.
fn shape(
    text: SharedString,
    runs: &[TextRun],
    width: Option<Pixels>,
    window: &Window,
) -> (Vec<WrappedLine>, Vec<Pixels>, Pixels) {
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let line_height = window.line_height();
    let lines: Vec<WrappedLine> = window
        .text_system()
        .shape_text(text, font_size, runs, width, None)
        .map(|lines| lines.into_iter().collect())
        .unwrap_or_default();
    let mut tops = Vec::with_capacity(lines.len());
    let mut y = Pixels::ZERO;
    for line in &lines {
        tops.push(y);
        y += line.size(line_height).height.max(line_height);
    }
    (lines, tops, y.max(line_height))
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let (text, runs) = display_runs(self.editor.read(cx), window, cx);
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, _| {
                let width = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let (_, _, height) = shape(text.clone(), &runs, width, window);
                size(width.unwrap_or(Pixels::ZERO), height)
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let editor = self.editor.read(cx);
        let (text, runs) = display_runs(editor, window, cx);
        let (lines, tops, _) = shape(text, &runs, Some(bounds.size.width), window);
        let line_height = window.line_height();
        let starts = if editor.text.is_empty() {
            vec![0; lines.len()]
        } else {
            let mut starts = Vec::with_capacity(lines.len());
            let mut at = 0;
            for line in &lines {
                starts.push(at);
                at += line.len() + 1;
            }
            starts
        };
        let colors = &cx.theme().colors;
        let layout = LayoutView {
            lines: &lines,
            starts: &starts,
            tops: &tops,
            bounds,
            line_height,
        };
        let (caret, selection) = if editor.text.is_empty() {
            let caret = fill(
                Bounds::new(bounds.origin, size(CARET_WIDTH, line_height)),
                colors.accent,
            );
            (Some(caret), Vec::new())
        } else if editor.selected.is_empty() {
            let at = layout.position_for_index(editor.cursor());
            (
                Some(fill(
                    Bounds::new(at, size(CARET_WIDTH, line_height)),
                    colors.accent,
                )),
                Vec::new(),
            )
        } else {
            let start = layout.position_for_index(editor.selected.start);
            let end = layout.position_for_index(editor.selected.end);
            let background = colors.accent.opacity(0.3);
            let mut quads = Vec::new();
            if (start.y - end.y).abs() < line_height / 2.0 {
                quads.push(fill(
                    Bounds::from_corners(start, point(end.x, end.y + line_height)),
                    background,
                ));
            } else {
                quads.push(fill(
                    Bounds::from_corners(start, point(bounds.right(), start.y + line_height)),
                    background,
                ));
                if end.y > start.y + line_height {
                    quads.push(fill(
                        Bounds::from_corners(
                            point(bounds.left(), start.y + line_height),
                            point(bounds.right(), end.y),
                        ),
                        background,
                    ));
                }
                quads.push(fill(
                    Bounds::from_corners(
                        point(bounds.left(), end.y),
                        point(end.x, end.y + line_height),
                    ),
                    background,
                ));
            }
            (None, quads)
        };
        EditorPrepaint {
            lines,
            starts,
            tops,
            line_height,
            selection,
            caret,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }
        let line_height = prepaint.line_height;
        for (line, top) in prepaint.lines.iter().zip(&prepaint.tops) {
            line.paint(
                bounds.origin + point(Pixels::ZERO, *top),
                line_height,
                gpui::TextAlign::Left,
                Some(bounds),
                window,
                cx,
            )
            .ok();
        }
        if focus_handle.is_focused(window)
            && let Some(caret) = prepaint.caret.take()
        {
            window.paint_quad(caret);
        }
        let lines = std::mem::take(&mut prepaint.lines);
        let starts = std::mem::take(&mut prepaint.starts);
        let tops = std::mem::take(&mut prepaint.tops);
        let has_text = !self.editor.read(cx).text.is_empty();
        self.editor.update(cx, |editor, _| {
            editor.last_layout = Some(LastLayout {
                // The placeholder is not the text: hit testing an empty
                // editor always lands on offset 0.
                lines: if has_text { lines } else { Vec::new() },
                starts,
                tops,
                bounds,
                line_height,
            });
        });
    }
}
