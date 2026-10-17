//! The "Comments" tab of the left dock: every thread of the review, grouped
//! by file, with outdated threads in their own group at the end (spec §7.3,
//! §9). Status chips filter the list; a click asks the workspace to scroll
//! the diff to the thread ([`CommentListEvent::Open`]).
//!
//! Rows are rendered with `uniform_list` and rebuilt only when the comments
//! or the filter change, so a review with thousands of comments stays
//! cheap.

use std::collections::HashSet;
use std::sync::Arc;

use argus_diff::review::{Comment, CommentId, CommentStatus};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, SharedString, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::comments::{ReviewComments, ReviewCommentsEvent};
use crate::components::{chip, status_style};
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

/// What the list reports to the workspace.
#[derive(Clone, Debug)]
pub enum CommentListEvent {
    /// Show this thread in the diff.
    Open(CommentId),
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
            hidden: HashSet::new(),
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
            cx.emit(CommentListEvent::Open(comment.id));
            cx.notify();
        }
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
                        let first = comment
                            .body_md
                            .lines()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or_default()
                            .to_owned();
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
        let chips = STATUSES.into_iter().map(|status| {
            let (label, color) = status_style(status, &colors);
            let count = self
                .snapshot
                .iter()
                .filter(|comment| comment.status == status)
                .count();
            let active = !self.hidden.contains(&status);
            let chip = if active {
                chip(label, color)
            } else {
                chip(label, colors.text_placeholder).opacity(0.6)
            };
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
            .when(!self.hidden.is_empty(), |row| {
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
            })
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
    }
}
