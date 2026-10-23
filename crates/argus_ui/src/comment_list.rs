//! The "Comments" tab of the left dock: every thread of the review, grouped
//! by file, with outdated threads in their own group at the end (spec §7.3,
//! §9). Status chips filter the list; a click asks the workspace to scroll
//! the diff to the thread ([`CommentListEvent::Open`]).
//!
//! Outdated threads are not inline in the diff. Selecting one opens a card
//! under the list with its stored snippet at the original line numbers and
//! its actions: Re-attach ([`CommentListEvent::Reattach`]), Dismiss, and a
//! disabled Run (outdated comments cannot run, spec Q5).
//!
//! Resolved and dismissed threads are filtered out at first ([`CLOSED`]):
//! the list shows what still needs the reviewer. Their chips bring them
//! back.
//!
//! Rows are rendered with `uniform_list` and rebuilt only when the comments
//! or the filter change, so a review with thousands of comments stays
//! cheap.

use std::collections::HashSet;
use std::sync::Arc;

use argus_diff::review::{Comment, CommentEvent, CommentId, CommentStatus};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, SharedString, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::comments::{ReviewComments, ReviewCommentsEvent};
use crate::components::{
    button, chip, disabled_button, primary_button, status_chip, status_style, text_tooltip,
};
use crate::markdown;
use crate::theme::ActiveTheme;

/// Height of every row (`uniform_list` needs one height).
const ROW_HEIGHT: f32 = 26.0;

/// The statuses the filter offers, in display order.
const STATUSES: [CommentStatus; 5] = [
    CommentStatus::Open,
    CommentStatus::Running,
    CommentStatus::Proposed,
    CommentStatus::Resolved,
    CommentStatus::Dismissed,
];

/// The statuses hidden until their chip is clicked: closed threads.
const CLOSED: [CommentStatus; 2] = [CommentStatus::Resolved, CommentStatus::Dismissed];

/// What the list reports to the workspace.
#[derive(Clone, Debug)]
pub enum CommentListEvent {
    /// Show this thread in the diff.
    Open(CommentId),
    /// Pick new lines in the diff for this outdated thread.
    Reattach(CommentId),
}

/// One visible row.
#[derive(Clone, Debug)]
enum Row {
    /// A file (or the "Outdated" group) with its number of shown threads.
    Group { title: SharedString, count: usize },
    /// A thread, by index in the snapshot.
    Comment(usize),
}

/// The comment list.
pub struct CommentList {
    comments: Entity<ReviewComments>,
    snapshot: Arc<Vec<Comment>>,
    /// Statuses hidden by the filter.
    hidden: HashSet<CommentStatus>,
    rows: Vec<Row>,
    selected: Option<CommentId>,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
}

impl EventEmitter<CommentListEvent> for CommentList {}

impl CommentList {
    /// A list following `comments`.
    pub fn new(comments: Entity<ReviewComments>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&comments, |this, _, event, cx| {
            if let ReviewCommentsEvent::Changed(_) = event {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            comments,
            snapshot: Arc::default(),
            hidden: HashSet::from(CLOSED),
            rows: Vec::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            _subscription: subscription,
        };
        this.refresh(cx);
        this
    }

    /// Number of threads in the review (whatever the filter).
    pub fn count(&self) -> usize {
        self.snapshot.len()
    }

    /// Number of threads in `status` (whatever the filter).
    pub fn status_count(&self, status: CommentStatus) -> usize {
        self.snapshot
            .iter()
            .filter(|comment| comment.status == status)
            .count()
    }

    /// Sets the filter to show `status` only (the status-bar counters).
    pub fn show_only(&mut self, status: CommentStatus, cx: &mut Context<Self>) {
        self.hidden = STATUSES
            .into_iter()
            .filter(|other| *other != status)
            .collect();
        self.rebuild_rows();
        cx.notify();
    }

    /// Marks `id` as the selected row (the thread shown in the diff).
    pub fn select(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.snapshot = self.comments.read(cx).comments().clone();
        self.rebuild_rows();
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        let mut outdated = Vec::new();
        let mut group: Option<(String, usize, usize)> = None;
        let shown = |comment: &Comment| !self.hidden.contains(&comment.status);
        for (index, comment) in self.snapshot.iter().enumerate() {
            if !shown(comment) {
                continue;
            }
            if comment.outdated {
                outdated.push(index);
                continue;
            }
            let path = &comment.anchor.file_path;
            if group.as_ref().is_none_or(|(current, ..)| current != path) {
                if let Some((title, at, count)) = group.take() {
                    rows[at] = Row::Group {
                        title: title.into(),
                        count,
                    };
                }
                group = Some((path.clone(), rows.len(), 0));
                rows.push(Row::Group {
                    title: SharedString::default(),
                    count: 0,
                });
            }
            if let Some((_, _, count)) = group.as_mut() {
                *count += 1;
            }
            rows.push(Row::Comment(index));
        }
        if let Some((title, at, count)) = group.take() {
            rows[at] = Row::Group {
                title: title.into(),
                count,
            };
        }
        if !outdated.is_empty() {
            rows.push(Row::Group {
                title: "Outdated".into(),
                count: outdated.len(),
            });
            rows.extend(outdated.into_iter().map(Row::Comment));
        }
        self.rows = rows;
    }

    fn toggle_status(&mut self, status: CommentStatus, cx: &mut Context<Self>) {
        if !self.hidden.remove(&status) {
            self.hidden.insert(status);
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn show_all(&mut self, cx: &mut Context<Self>) {
        self.hidden.clear();
        self.rebuild_rows();
        cx.notify();
    }

    fn open(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(comment) = self.snapshot.get(index) {
            self.selected = Some(comment.id);
            // Outdated threads have no place in the diff; their card shows
            // below the list instead.
            if !comment.outdated {
                cx.emit(CommentListEvent::Open(comment.id));
            }
            cx.notify();
        }
    }

    fn dismiss(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.comments
            .update(cx, |comments, cx| {
                comments.apply(id, CommentEvent::Dismissed, cx)
            })
            .detach();
    }

    /// The card of the selected thread when it is outdated: its stored
    /// snippet with the original line numbers, and its actions.
    fn render_outdated(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let comment = self
            .selected
            .and_then(|id| self.snapshot.iter().find(|comment| comment.id == id))
            .filter(|comment| comment.outdated)?;
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let id = comment.id;
        let anchor = &comment.anchor;
        let lines = if anchor.is_range() {
            format!("lines {}–{}", anchor.start_line, anchor.end_line)
        } else {
            format!("line {}", anchor.start_line)
        };
        let first_line = anchor.snippet_start_line();
        let digits = (first_line as usize + anchor.snippet.lines().count())
            .to_string()
            .len();
        let snippet = anchor
            .snippet
            .split('\n')
            .enumerate()
            .map(|(offset, text)| {
                let number = first_line + offset as u32;
                let commented = anchor.lines().contains(&number);
                div()
                    .flex()
                    .gap_2()
                    .px_1()
                    .when(commented, |row| row.bg(colors.warning.opacity(0.12)))
                    .child(
                        div()
                            .flex_none()
                            .text_color(colors.text_placeholder)
                            .child(format!("{number:>digits$}")),
                    )
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_color(if commented {
                                colors.text
                            } else {
                                colors.text_muted
                            })
                            .child(text.to_owned()),
                    )
            });
        let body = comment
            .body_md
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        let dismiss = match comment.status {
            CommentStatus::Open | CommentStatus::Proposed => {
                button("outdated-dismiss", "Dismiss", cx)
                    .text_xs()
                    .on_click(cx.listener(move |this, _, _, cx| this.dismiss(id, cx)))
                    .into_any_element()
            }
            _ => disabled_button("outdated-dismiss", "Dismiss", cx)
                .text_xs()
                .tooltip(text_tooltip(format!(
                    "A {} comment cannot be dismissed.",
                    comment.status
                )))
                .into_any_element(),
        };
        Some(
            div()
                .id("outdated-card")
                .flex()
                .flex_col()
                .flex_none()
                .gap_1p5()
                .max_h(px(360.0))
                .p_2()
                .border_t_1()
                .border_color(colors.border)
                .bg(colors.elevated_surface)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(chip("Outdated", colors.warning))
                        .child(status_chip(comment.status, colors))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_xs()
                                .text_color(colors.text_muted)
                                .font_family(theme.fonts.code_family.clone())
                                .child(format!("{} · {lines}", anchor.file_path)),
                        ),
                )
                .child(
                    div()
                        .id("outdated-snippet")
                        .flex()
                        .flex_col()
                        .min_h_0()
                        .overflow_scroll()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.background)
                        .text_xs()
                        .font_family(theme.fonts.code_family.clone())
                        .children(snippet),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(colors.text)
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(body),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .child(
                            primary_button("outdated-reattach", "Re-attach…", cx)
                                .text_xs()
                                .tooltip(text_tooltip("Pick the new lines in the diff, then Save."))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(CommentListEvent::Reattach(id))
                                })),
                        )
                        .child(dismiss)
                        .child(
                            disabled_button("outdated-run", "Run", cx)
                                .text_xs()
                                .tooltip(text_tooltip(
                                    "Outdated comments cannot be run. Re-attach it to the \
                                     current code first.",
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        range
            .filter_map(|row_index| {
                let row = self.rows.get(row_index)?.clone();
                let element = match row {
                    Row::Group { title, count } => div()
                        .id(("comment-group", row_index))
                        .flex()
                        .items_end()
                        .justify_between()
                        .gap_2()
                        .h(px(ROW_HEIGHT))
                        .px_3()
                        .pb_0p5()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .font_family(theme.fonts.code_family.clone())
                                .child(title),
                        )
                        .child(div().flex_none().child(count.to_string()))
                        .into_any_element(),
                    Row::Comment(index) => {
                        let comment = self.snapshot.get(index)?;
                        let (_, color) = status_style(comment.status, colors);
                        let selected = self.selected == Some(comment.id);
                        let anchor = &comment.anchor;
                        let line = if anchor.is_range() {
                            format!("L{}–{}", anchor.start_line, anchor.end_line)
                        } else {
                            format!("L{}", anchor.start_line)
                        };
                        let title = if comment.outdated {
                            format!("{} {line}", anchor.file_path)
                        } else {
                            line
                        };
                        let first = markdown::first_line_plain(&comment.body_md);
                        let replies = comment.replies.len();
                        div()
                            .id(("comment-row", row_index))
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(ROW_HEIGHT))
                            .px_2()
                            .mx_1()
                            .rounded_md()
                            .cursor_pointer()
                            .when(selected, |row| row.bg(colors.element_selected))
                            .hover(|style| style.bg(colors.element_hover))
                            .on_click(cx.listener(move |this, _, _, cx| this.open(index, cx)))
                            // Status dot, coloured like the thread's chip.
                            .child(div().flex_none().size(px(8.0)).rounded_full().bg(color))
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(colors.text_muted)
                                    .font_family(theme.fonts.code_family.clone())
                                    .child(title),
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
                                    .child(first),
                            )
                            .when(replies > 0, |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(colors.text_placeholder)
                                        .child(format!("↩{replies}")),
                                )
                            })
                            .into_any_element()
                    }
                };
                Some(element)
            })
            .collect()
    }

    fn render_filter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        // A status no thread is in has nothing to filter: no chip.
        let chips = STATUSES.into_iter().filter_map(|status| {
            let (label, color) = status_style(status, &colors);
            let count = self
                .snapshot
                .iter()
                .filter(|comment| comment.status == status)
                .count();
            if count == 0 {
                return None;
            }
            let active = !self.hidden.contains(&status);
            let chip = if active {
                chip(label, color)
            } else {
                chip(label, colors.text_placeholder).opacity(0.6)
            };
            Some(
                div()
                    .id(SharedString::from(format!("comment-filter-{status}")))
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_status(status, cx)))
                    .child(chip)
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.text_placeholder)
                            .child(count.to_string()),
                    ),
            )
        });
        div()
            .flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap_1p5()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(colors.border_variant)
            .children(chips)
            // Only when the filter leaves a thread out.
            .when(
                self.snapshot
                    .iter()
                    .any(|comment| self.hidden.contains(&comment.status)),
                |row| {
                    row.child(
                        div()
                            .id("comment-filter-all")
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(colors.accent)
                            .hover(|style| style.bg(colors.element_hover))
                            .on_click(cx.listener(|this, _, _, cx| this.show_all(cx)))
                            .child("Show all"),
                    )
                },
            )
    }
}

impl Render for CommentList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let loading = self.comments.read(cx).is_loading();
        let body = if self.rows.is_empty() {
            let text = if loading {
                "Loading comments…"
            } else if self.snapshot.is_empty() {
                "No comments yet. Hover a line and click “+”, or press c."
            } else if self.snapshot.iter().all(Comment::is_closed) {
                "Nothing left to review."
            } else {
                "No comment matches the filter."
            };
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .justify_center()
                .p_4()
                .text_sm()
                .text_center()
                .text_color(colors.text_placeholder)
                .child(text)
                .into_any_element()
        } else {
            uniform_list(
                "comment-list-rows",
                self.rows.len(),
                cx.processor(|this, range, _, cx| this.render_rows(range, cx)),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
            .py_1()
            .into_any_element()
        };
        div()
            .id("comment-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .font_weight(FontWeight::NORMAL)
            .child(self.render_filter(cx))
            .child(body)
            .children(self.render_outdated(cx))
    }
}
