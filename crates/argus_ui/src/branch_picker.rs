//! The `base ▾` / `head ▾` popover: a fuzzy-searchable branch list grouped
//! Local / Remote, with the tip commit time as secondary text.
//!
//! Rows are rendered with `uniform_list`, so only the visible ones are laid
//! out; filtering is a single allocation-free pass per candidate. Together
//! that keeps typing smooth with thousands of branches.

use std::sync::Arc;

use argus_git::{Branch, BranchKind};
use gpui::{
    ClickEvent, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton,
    ScrollStrategy, SharedString, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};

use crate::components::radius;
use crate::fuzzy::FuzzyQuery;
use crate::keymap::{Dismiss, TEXT_INPUT_CONTEXT};
use crate::session::{Side, now_seconds, relative_time};
use crate::theme::ActiveTheme;

/// Popover width.
const PICKER_WIDTH: f32 = 380.0;
/// Height of the list area.
const LIST_HEIGHT: f32 = 320.0;
/// Height of one row (headers included, `uniform_list` needs one height).
const ROW_HEIGHT: f32 = 26.0;

/// What the picker reports to the workspace.
#[derive(Clone, Debug)]
pub enum PickerEvent {
    /// The user chose a branch (short name).
    Picked(Side, String),
    /// The picker was closed without a choice.
    Dismissed,
}

/// One visible row.
#[derive(Clone, Copy, Debug)]
enum Row {
    /// A group title with the number of matching branches.
    Header(BranchKind, usize),
    /// An index into the branch list.
    Branch(usize),
}

/// The branch picker popover.
pub struct BranchPicker {
    side: Side,
    branches: Arc<Vec<Branch>>,
    selected: Option<String>,
    query: String,
    rows: Vec<Row>,
    /// Index into `rows` of the keyboard-highlighted branch row.
    highlighted: Option<usize>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    now: i64,
}

impl EventEmitter<PickerEvent> for BranchPicker {}

impl Focusable for BranchPicker {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BranchPicker {
    /// A picker for `side` over `branches`, highlighting the current choice.
    pub fn new(
        side: Side,
        branches: Arc<Vec<Branch>>,
        selected: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            side,
            branches,
            selected,
            query: String::new(),
            rows: Vec::new(),
            highlighted: None,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            now: now_seconds(),
        };
        this.update_matches();
        if let Some(row) = this.selected_row() {
            this.highlighted = Some(row);
            this.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
        this
    }

    /// Which side the picker edits.
    pub fn side(&self) -> Side {
        self.side
    }

    fn selected_row(&self) -> Option<usize> {
        let selected = self.selected.as_deref()?;
        self.rows.iter().position(|row| match row {
            Row::Branch(index) => self.branches[*index].name == selected,
            Row::Header(..) => false,
        })
    }

    /// Recomputes the visible rows for the query.
    fn update_matches(&mut self) {
        let query = FuzzyQuery::new(&self.query);
        let mut rows = Vec::new();
        for kind in [BranchKind::Local, BranchKind::Remote] {
            let mut matches: Vec<(i32, usize)> = self
                .branches
                .iter()
                .enumerate()
                .filter(|(_, branch)| branch.kind == kind)
                .filter_map(|(index, branch)| Some((query.score(&branch.name)?, index)))
                .collect();
            if matches.is_empty() {
                continue;
            }
            if !query.is_empty() {
                let branches = &self.branches;
                matches.sort_by(|(score_a, a), (score_b, b)| {
                    score_b
                        .cmp(score_a)
                        .then_with(|| branches[*a].name.len().cmp(&branches[*b].name.len()))
                        .then_with(|| branches[*a].name.cmp(&branches[*b].name))
                });
            }
            rows.push(Row::Header(kind, matches.len()));
            rows.extend(matches.into_iter().map(|(_, index)| Row::Branch(index)));
        }
        self.rows = rows;
        self.highlighted = self.first_branch_row();
        if let Some(row) = self.highlighted {
            self.scroll.scroll_to_item(row, ScrollStrategy::Top);
        }
    }

    fn first_branch_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Branch(_)))
    }

    /// Moves the highlight to the next (`delta > 0`) or previous branch row.
    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(current) = self.highlighted.or_else(|| self.first_branch_row()) else {
            return;
        };
        let mut row = current as isize;
        loop {
            row += delta;
            if row < 0 || row as usize >= self.rows.len() {
                return;
            }
            if matches!(self.rows[row as usize], Row::Branch(_)) {
                break;
            }
        }
        self.highlighted = Some(row as usize);
        self.scroll
            .scroll_to_item(row as usize, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn pick_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(Row::Branch(index)) = self.rows.get(row) {
            let name = self.branches[*index].name.clone();
            cx.emit(PickerEvent::Picked(self.side, name));
        }
    }

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if query != self.query {
            self.query = query;
            self.update_matches();
            cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        match keystroke.key.as_str() {
            "up" => self.move_highlight(-1, cx),
            "down" => self.move_highlight(1, cx),
            "pageup" => self.move_highlight(-10, cx),
            "pagedown" => self.move_highlight(10, cx),
            "enter" => {
                if let Some(row) = self.highlighted {
                    self.pick_row(row, cx);
                }
            }
            "backspace" if modifiers.secondary() || modifiers.alt => {
                self.set_query(String::new(), cx);
            }
            "backspace" => {
                let mut query = self.query.clone();
                query.pop();
                self.set_query(query, cx);
            }
            "v" if modifiers.secondary() => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let line = text.lines().next().unwrap_or_default().trim();
                    let query = format!("{}{line}", self.query);
                    self.set_query(query, cx);
                }
            }
            _ => {
                if modifiers.control || modifiers.platform || modifiers.function {
                    return;
                }
                let Some(text) = keystroke.key_char.as_deref() else {
                    return;
                };
                if text.chars().any(char::is_control) {
                    return;
                }
                let query = format!("{}{text}", self.query);
                self.set_query(query, cx);
            }
        }
        cx.stop_propagation();
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PickerEvent::Dismissed);
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let colors = cx.theme().colors.clone();
        let mut elements = Vec::with_capacity(range.len());
        for row_index in range {
            let Some(row) = self.rows.get(row_index).copied() else {
                break;
            };
            let element = match row {
                Row::Header(kind, count) => div()
                    .id(("branch-header", row_index))
                    .flex()
                    .items_end()
                    .justify_between()
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .pb_0p5()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(match kind {
                        BranchKind::Local => "LOCAL",
                        BranchKind::Remote => "REMOTE",
                    })
                    .child(count.to_string())
                    .into_any_element(),
                Row::Branch(index) => {
                    let branch = &self.branches[index];
                    let highlighted = self.highlighted == Some(row_index);
                    let current = self.selected.as_deref() == Some(branch.name.as_str());
                    let time: SharedString = branch
                        .tip_time
                        .map(|time| relative_time(time, self.now))
                        .unwrap_or_default()
                        .into();
                    div()
                        .id(("branch-row", row_index))
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(ROW_HEIGHT))
                        .px_3()
                        .mx_1()
                        .rounded(radius::sm(cx))
                        .cursor_pointer()
                        .when(highlighted, |row| row.bg(colors.element_selected))
                        .hover(|style| style.bg(colors.element_hover))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.pick_row(row_index, cx);
                        }))
                        .child(
                            div()
                                .w(px(12.0))
                                .flex_none()
                                .text_xs()
                                .text_color(colors.accent)
                                .child(if current { "✓" } else { "" }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_sm()
                                .text_color(colors.text)
                                .child(branch.name.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(colors.text_muted)
                                .child(time),
                        )
                        .into_any_element()
                }
            };
            elements.push(element);
        }
        elements
    }
}

impl Render for BranchPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let focused = self.focus_handle.is_focused(window);
        let accent = colors.accent;
        // A plain caret, since there is no text-input element yet.
        let caret = move || div().flex_none().w(px(1.5)).h(px(16.0)).bg(accent);
        let hint = match self.side {
            Side::Base => "Filter base branches…",
            Side::Head => "Filter head branches…",
        };
        let query_text: SharedString = if self.query.is_empty() {
            hint.into()
        } else {
            self.query.clone().into()
        };

        let body = if self.rows.is_empty() {
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(64.0))
                .text_sm()
                .text_color(colors.text_placeholder)
                .child(if self.branches.is_empty() {
                    "No branches."
                } else {
                    "No branch matches."
                })
                .into_any_element()
        } else {
            uniform_list(
                "branch-list",
                self.rows.len(),
                cx.processor(|this, range, _, cx| this.render_rows(range, cx)),
            )
            .track_scroll(&self.scroll)
            .h(px(LIST_HEIGHT.min(self.rows.len() as f32 * ROW_HEIGHT)))
            .py_1()
            .into_any_element()
        };

        div()
            .id("branch-picker")
            .key_context(TEXT_INPUT_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(PickerEvent::Dismissed)))
            // Clicks inside must not reach the title bar's drag area.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .occlude()
            .flex()
            .flex_col()
            .w(px(PICKER_WIDTH))
            .rounded(radius::md(cx))
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface)
            .shadow_lg()
            .overflow_hidden()
            .font_family(theme.fonts.ui_family.clone())
            .text_size(theme.fonts.ui_size)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .h(px(34.0))
                    .px_3()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .text_sm()
                    .when(focused && self.query.is_empty(), |row| row.child(caret()))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(if self.query.is_empty() {
                                colors.text_placeholder
                            } else {
                                colors.text
                            })
                            .child(query_text),
                    )
                    .when(focused && !self.query.is_empty(), |row| row.child(caret())),
            )
            .child(body)
    }
}
