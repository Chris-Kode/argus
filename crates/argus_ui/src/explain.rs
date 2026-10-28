//! Claude explains the change: the title bar's Explain button (the whole
//! diff) and "Explain selection" in the diff (lines of one file).
//!
//! [`ExplainDialog`] shows the explanation as Markdown: in short, before,
//! after, step by step, diagrams, what is worth checking. A stored
//! explanation of the same merge-base, head tip and scope opens at once
//! ([`argus_runs::RunManager::cached_explanation`]); otherwise one is
//! written with the explanation model and effort, streamed into the dialog
//! as it arrives. Closing the dialog while Claude writes stops it.
//!
//! The model and effort are their own defaults, apart from the Run menu's:
//! [`choice_menu`] (under the title bar's Explain button) and
//! [`choice_buttons`] (in the dialog) change them. The lookup and the call
//! run on the tokio runtime; this view only receives the results.

use std::sync::Arc;

use argus_runs::settings::{EFFORTS, MODELS};
use argus_runs::{ExplainEvent, ExplainRequest, ExplainTicket};
use argus_store::Explanation;
use gpui::{
    AnyElement, App, Context, Div, EventEmitter, FontWeight, MouseButton, ScrollHandle,
    SharedString, Task, Window, div, prelude::*, px, relative,
};

use crate::AppContext;
use crate::components::{button, claude_activity, kbd, primary_button, radius};
use crate::followup::{self, next_in};
use crate::icons::{IconName, icon};
use crate::markdown::{self, Markdown, MarkdownCache};
use crate::runtime::spawn_job;
use crate::theme::ActiveTheme;

/// Widest the dialog gets.
const DIALOG_WIDTH: f32 = 860.0;
/// Tallest the dialog gets.
const DIALOG_HEIGHT: f32 = 820.0;
/// Widest line of the explanation text.
const TEXT_WIDTH: f32 = 720.0;

/// The explanation model and effort: the last choice.
pub fn choice(cx: &App) -> (String, String) {
    let settings = cx.global::<AppContext>().runs.settings();
    (settings.explain_model, settings.explain_effort)
}

/// Makes `model` and/or `effort` the explanation defaults; the settings
/// are saved off the UI thread.
fn pick(model: Option<String>, effort: Option<String>, cx: &mut App) {
    let (current_model, current_effort) = choice(cx);
    let app = cx.global::<AppContext>();
    let runs = app.runs.clone();
    let model = model.unwrap_or(current_model);
    let effort = effort.unwrap_or(current_effort);
    if runs.remember_explain_choice(&model, &effort) {
        app.runtime.spawn(async move {
            if let Err(error) = runs.save_settings().await {
                tracing::warn!(%error, "cannot save the explanation defaults");
            }
        });
    }
}

/// The model and effort buttons of the dialog; a click picks the next
/// offered entry.
pub fn choice_buttons<V: 'static>(id: &str, cx: &mut Context<V>) -> Div {
    let (model, effort) = choice(cx);
    let colors = cx.theme().colors.clone();
    let model_button = button(SharedString::from(format!("{id}-model")), model.clone(), cx)
        .h(px(20.0))
        .text_xs()
        .text_color(colors.text_muted)
        .on_click(cx.listener(move |_, _, _, cx| {
            pick(next_in(&MODELS, &model), None, cx);
            cx.notify();
        }));
    let effort_button = (!effort.trim().is_empty()).then(|| {
        button(
            SharedString::from(format!("{id}-effort")),
            effort.clone(),
            cx,
        )
        .h(px(20.0))
        .text_xs()
        .text_color(colors.text_muted)
        .on_click(cx.listener(move |_, _, _, cx| {
            pick(None, next_in(&EFFORTS, &effort), cx);
            cx.notify();
        }))
    });
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_0p5()
        .child(model_button)
        .children(effort_button)
}

/// The explanation's model and effort menu: every model and effort on
/// offer, the current ones checked. `close` runs on a click outside it.
pub fn choice_menu<V: 'static>(
    id: &'static str,
    close: impl Fn(&mut V, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let colors = cx.theme().colors.clone();
    let (model, effort) = choice(cx);
    let section = |title: &'static str| {
        div()
            .px_2()
            .pt_1()
            .text_xs()
            .text_color(colors.text_placeholder)
            .child(title)
    };
    let option = |index: usize, label: &str, selected: bool| {
        div()
            .id((id, index))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_0p5()
            .rounded_sm()
            .whitespace_nowrap()
            .cursor_pointer()
            .text_color(colors.text)
            .hover(|style| style.bg(colors.element_hover))
            .child(
                div()
                    .w(px(12.0))
                    .flex_none()
                    .text_color(colors.accent)
                    .child(if selected { "✓" } else { "" }),
            )
            .child(label.to_owned())
    };
    let models = MODELS.iter().enumerate().map(|(index, &name)| {
        option(index, name, name == model).on_click(cx.listener(move |_, _, _, cx| {
            pick(Some(name.to_owned()), None, cx);
            cx.notify();
        }))
    });
    let efforts = EFFORTS.iter().enumerate().map(|(index, &name)| {
        option(MODELS.len() + index, name, name == effort).on_click(cx.listener(
            move |_, _, _, cx| {
                pick(None, Some(name.to_owned()), cx);
                cx.notify();
            },
        ))
    });
    let effort_note = cx
        .global::<AppContext>()
        .runs
        .claude_status()
        .warning()
        .map(|_| {
            div()
                .px_2()
                .text_xs()
                .text_color(colors.warning)
                .child("This Claude Code ignores the effort.")
        });
    div()
        .id((id, 999_usize))
        .occlude()
        .flex()
        .flex_col()
        .min_w(px(160.0))
        .p_1()
        .rounded(radius::sm(cx))
        .border_1()
        .border_color(colors.border)
        .bg(colors.elevated_surface)
        .shadow_lg()
        .text_sm()
        .on_mouse_down_out(cx.listener(move |view, _, _, cx| close(view, cx)))
        .child(section("MODEL"))
        .children(models)
        .child(section("EFFORT"))
        .children(efforts)
        .children(effort_note)
        .into_any_element()
}

/// Events of the dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplainDialogEvent {
    /// The user closed the dialog.
    Closed,
}

/// What the dialog shows.
enum State {
    /// Looking for a stored explanation.
    Looking,
    /// Claude is writing; `text` is what arrived so far.
    Writing { text: String, doc: Arc<Markdown> },
    /// The explanation.
    Ready(Explanation),
    /// Why there is none.
    Failed(SharedString),
}

/// The explanation of the whole diff or of a line selection.
pub struct ExplainDialog {
    request: ExplainRequest,
    title: SharedString,
    state: State,
    /// The explanation shown before Regenerate; Stop goes back to it.
    previous: Option<Explanation>,
    markdown: MarkdownCache,
    scroll: ScrollHandle,
    task: Option<Task<()>>,
}

impl EventEmitter<ExplainDialogEvent> for ExplainDialog {}

impl ExplainDialog {
    /// Opens the stored explanation of `request`, or writes one.
    pub fn new(request: ExplainRequest, title: SharedString, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            request,
            title,
            state: State::Looking,
            previous: None,
            markdown: MarkdownCache::default(),
            scroll: ScrollHandle::new(),
            task: None,
        };
        this.look_up(cx);
        this
    }

    /// Whether this dialog explains `request`'s diff and scope.
    pub fn explains(&self, request: &ExplainRequest) -> bool {
        self.request.key() == request.key()
    }

    fn look_up(&mut self, cx: &mut Context<Self>) {
        let runs = cx.global::<AppContext>().runs.clone();
        let request = self.request.clone();
        let job = spawn_job(cx, async move { runs.cached_explanation(&request).await });
        self.task = Some(cx.spawn(async move |this, cx| {
            let cached = match job.await {
                Ok(Ok(cached)) => cached,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "cannot read the stored explanation");
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "explanation lookup did not finish");
                    None
                }
            };
            this.update(cx, |this, cx| match cached {
                Some(explanation) => {
                    this.state = State::Ready(explanation);
                    this.task = None;
                    cx.notify();
                }
                None => this.write(cx),
            })
            .ok();
        }));
    }

    /// Has Claude write the explanation with the current model and effort.
    fn write(&mut self, cx: &mut Context<Self>) {
        let (model, effort) = choice(cx);
        let runs = cx.global::<AppContext>().runs.clone();
        let request = self.request.clone();
        let job = spawn_job(cx, async move { runs.explain(request, model, effort) });
        if let State::Ready(explanation) = std::mem::replace(&mut self.state, State::Looking) {
            self.previous = Some(explanation);
        }
        self.state = State::Writing {
            text: String::new(),
            doc: Arc::default(),
        };
        self.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut ticket: ExplainTicket = match job.await {
                Ok(Ok(ticket)) => ticket,
                Ok(Err(error)) => {
                    let message = error.to_string();
                    this.update(cx, |this, cx| this.fail(message, cx)).ok();
                    return;
                }
                Err(error) => {
                    let message = error.to_string();
                    this.update(cx, |this, cx| this.fail(message, cx)).ok();
                    return;
                }
            };
            loop {
                let Some(first) = ticket.events.recv().await else {
                    this.update(cx, |this, cx| {
                        this.fail("Claude stopped before answering".to_owned(), cx)
                    })
                    .ok();
                    return;
                };
                // Deltas come in bursts; one repaint per burst.
                let mut batch = vec![first];
                while let Ok(event) = ticket.events.try_recv() {
                    batch.push(event);
                }
                match this.update(cx, |this, cx| this.apply(batch, cx)) {
                    Ok(false) => {}
                    Ok(true) | Err(_) => return,
                }
            }
        }));
        cx.notify();
    }

    /// Applies events in order; `true` once the explanation is done.
    fn apply(&mut self, events: Vec<ExplainEvent>, cx: &mut Context<Self>) -> bool {
        let mut grew = false;
        for event in events {
            match event {
                ExplainEvent::Delta(delta) => {
                    if let State::Writing { text, .. } = &mut self.state {
                        text.push_str(&delta);
                        grew = true;
                    }
                }
                ExplainEvent::Done(explanation) => {
                    self.state = State::Ready(explanation);
                    self.previous = None;
                    self.task = None;
                    cx.notify();
                    return true;
                }
                ExplainEvent::Failed(message) => {
                    self.fail(message, cx);
                    return true;
                }
            }
        }
        if grew && let State::Writing { text, doc, .. } = &mut self.state {
            // Code blocks are highlighted once the answer is complete.
            *doc = Arc::new(Markdown::parse(text));
        }
        cx.notify();
        false
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.state = State::Failed(message.into());
        self.task = None;
        cx.notify();
    }

    /// Stops Claude: back to the explanation shown before, or closed.
    fn stop(&mut self, cx: &mut Context<Self>) {
        self.task = None;
        match self.previous.take() {
            Some(explanation) => {
                self.state = State::Ready(explanation);
                cx.notify();
            }
            None => cx.emit(ExplainDialogEvent::Closed),
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.task = None;
        cx.emit(ExplainDialogEvent::Closed);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        // While Claude writes, the footer's choice is the model at work.
        let meta = match &self.state {
            State::Ready(explanation) => {
                Some(followup::label(&explanation.model, &explanation.effort))
            }
            State::Looking | State::Writing { .. } | State::Failed(_) => None,
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .h(px(40.0))
            .px_4()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(icon(IconName::Sparkle, px(14.0), colors.claude))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child(self.title.clone()),
            )
            .children(meta.map(|meta| {
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(meta)
            }))
            .child(
                kbd("Esc", cx)
                    .id("explain-close")
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
            .into_any_element()
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let waiting = |label: &'static str, cx: &App| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(colors.text_muted)
                .child(claude_activity(px(16.0), cx))
                .child(label)
                .into_any_element()
        };
        let content = match &self.state {
            State::Looking => div().into_any_element(),
            State::Writing { text, .. } if text.trim().is_empty() => {
                waiting("Claude is reading the change…", cx)
            }
            State::Writing { doc, .. } => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(markdown::render_document(doc, "explain-writing".into(), cx))
                .child(claude_activity(px(14.0), cx))
                .into_any_element(),
            State::Ready(explanation) => {
                let body = explanation.body_md.clone();
                let doc = self
                    .markdown
                    .prepare(&body, |this: &mut Self| &mut this.markdown, cx);
                markdown::render_document(&doc, "explain".into(), cx)
            }
            State::Failed(message) => div()
                .px_2()
                .py_1()
                .rounded(radius::sm(cx))
                .border_1()
                .border_color(colors.error.opacity(0.6))
                .bg(colors.error.opacity(0.08))
                .text_sm()
                .text_color(colors.error)
                .child(format!("No explanation: {message}"))
                .into_any_element(),
        };
        div()
            .id("explain-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px_6()
            .py_4()
            .child(
                div()
                    .w_full()
                    .max_w(px(TEXT_WIDTH))
                    .mx_auto()
                    .line_height(relative(1.55))
                    .text_color(colors.text)
                    .child(content),
            )
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let action = match &self.state {
            State::Looking => None,
            State::Writing { .. } => Some(
                button("explain-stop", "Stop", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
                    .into_any_element(),
            ),
            State::Ready(_) => Some(
                button("explain-again", "Regenerate", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.write(cx)))
                    .into_any_element(),
            ),
            State::Failed(_) => Some(
                primary_button("explain-retry", "Try again", cx)
                    .h(px(24.0))
                    .on_click(cx.listener(|this, _, _, cx| this.write(cx)))
                    .into_any_element(),
            ),
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(colors.border_variant)
            .child(choice_buttons("explain-choice", cx))
            .child(div().flex_1())
            .children(action)
            .into_any_element()
    }
}

impl Render for ExplainDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let header = self.render_header(cx);
        let body = self.render_body(cx);
        let footer = self.render_footer(cx);
        div()
            .id("explain-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(colors.overlay)
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close(cx)),
            )
            .child(
                div()
                    .id("explain-dialog")
                    .flex()
                    .flex_col()
                    .w(relative(0.85))
                    .max_w(px(DIALOG_WIDTH))
                    .h(relative(0.88))
                    .max_h(px(DIALOG_HEIGHT))
                    .rounded(radius::md(cx))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(header)
                    .child(body)
                    .child(footer),
            )
    }
}
