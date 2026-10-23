//! The permission dialog: a live run waits for the reviewer.
//!
//! Runs work in `auto` permission mode, so Claude Code settles most tool
//! calls on its own; the rest come here as a [`PermissionRequest`] (see
//! [`argus_runs::permission`]). A tool call offers **Allow**, **Allow for
//! this run** (when Claude Code proposes a rule to remember) and **Deny**,
//! with an optional note telling Claude what to do instead. An
//! `AskUserQuestion` call shows its questions with their options and an
//! "Other" answer, and offers **Answer** or **Decline**. **Later** (or
//! Escape) hides the dialog; the run keeps waiting and the status bar's
//! "waiting for you" counter brings it back.

use argus_runs::{PermissionDecision, PermissionRequest};
use argus_store::RunId;
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, MouseButton, SharedString, Window, div,
    prelude::*, px,
};

use crate::AppContext;
use crate::components::{button, disabled_button, kbd, primary_button, toggle_button};
use crate::text_field::TextField;
use crate::theme::ActiveTheme;

/// What the dialog tells the workspace.
#[derive(Clone, Debug)]
pub enum PermissionDialogEvent {
    /// The request was answered; the next one, if any, takes its place.
    Answered,
    /// The reviewer chose Later: hide the dialog, the run keeps waiting.
    Later,
    /// A message for the status bar.
    Notice {
        /// Text to show.
        message: SharedString,
        /// Error (red) rather than information.
        is_error: bool,
    },
}

/// The dialog of one waiting request.
pub struct PermissionDialog {
    request: PermissionRequest,
    /// Where the run works, from the run panel.
    context: Option<String>,
    /// Requests waiting, this one included.
    waiting: usize,
    /// Picked options, per question.
    picks: Vec<Vec<usize>>,
    /// The "Other" answer, per question.
    others: Vec<Entity<TextField>>,
    /// The note sent with Deny.
    note: Entity<TextField>,
}

impl EventEmitter<PermissionDialogEvent> for PermissionDialog {}

impl PermissionDialog {
    /// A dialog for `request`, one of `waiting` requests.
    pub fn new(
        request: PermissionRequest,
        context: Option<String>,
        waiting: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        let others: Vec<Entity<TextField>> = request
            .questions
            .iter()
            .map(|_| cx.new(|cx| TextField::new("Other answer (optional)", cx)))
            .collect();
        let note =
            cx.new(|cx| TextField::new("Tell Claude what to do instead (sent with Deny)", cx));
        for field in std::iter::once(&note).chain(&others) {
            cx.observe(field, |_, _, cx| cx.notify()).detach();
        }
        Self {
            picks: vec![Vec::new(); request.questions.len()],
            request,
            context,
            waiting,
            others,
            note,
        }
    }

    /// The run and request id this dialog answers.
    pub fn key(&self) -> (RunId, &str) {
        (self.request.run_id, &self.request.request_id)
    }

    /// Updates the number of waiting requests.
    pub fn set_waiting(&mut self, waiting: usize, cx: &mut Context<Self>) {
        if waiting != self.waiting {
            self.waiting = waiting;
            cx.notify();
        }
    }

    fn answer(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        let request = &self.request;
        // Only a channel send: the run's stdin writer does the I/O.
        let sent = cx.global::<AppContext>().runs.answer_permission(
            request.run_id,
            &request.request_id,
            &decision,
        );
        if !sent {
            cx.emit(PermissionDialogEvent::Notice {
                message: "That request is no longer waiting: Claude withdrew it or its run ended."
                    .into(),
                is_error: true,
            });
        }
        cx.emit(PermissionDialogEvent::Answered);
    }

    fn deny(&mut self, cx: &mut Context<Self>) {
        let message = Some(self.note.read(cx).text().to_owned());
        self.answer(PermissionDecision::Deny { message }, cx);
    }

    fn toggle(&mut self, question: usize, option: usize, cx: &mut Context<Self>) {
        let multi = self
            .request
            .questions
            .get(question)
            .is_some_and(|q| q.multi_select);
        let Some(picks) = self.picks.get_mut(question) else {
            return;
        };
        if let Some(index) = picks.iter().position(|&picked| picked == option) {
            picks.remove(index);
        } else if multi {
            picks.push(option);
            picks.sort_unstable();
        } else {
            *picks = vec![option];
        }
        cx.notify();
    }

    /// The answer of every question (picked labels and the "Other" text,
    /// joined with `", "`), or `None` while one is unanswered.
    fn answers(&self, cx: &Context<Self>) -> Option<Vec<(String, String)>> {
        self.request
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let mut parts: Vec<String> = self.picks[index]
                    .iter()
                    .filter_map(|&option| question.options.get(option))
                    .map(|option| option.label.clone())
                    .collect();
                let other = self.others[index].read(cx).text().trim();
                if !other.is_empty() {
                    parts.push(other.to_owned());
                }
                (!parts.is_empty()).then(|| (question.question.clone(), parts.join(", ")))
            })
            .collect()
    }

    // ---- rendering ----------------------------------------------------------

    fn render_tool(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let request = &self.request;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(
                request
                    .description
                    .clone()
                    .map(|description| div().text_sm().text_color(colors.text).child(description)),
            )
            .child(
                div()
                    .id("permission-input")
                    .max_h(px(240.0))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.background)
                    .font_family(theme.fonts.code_family.clone())
                    .text_xs()
                    .text_color(colors.text)
                    .child(request.input_preview()),
            )
            .children(request.blocked_path.clone().map(|path| {
                div()
                    .text_xs()
                    .text_color(colors.warning)
                    .child(format!("Outside the run's worktree: {path}"))
            }))
            .child(self.note.clone())
            .into_any_element()
    }

    fn render_questions(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let questions = self
            .request
            .questions
            .iter()
            .enumerate()
            .map(|(q, question)| {
                let options = question.options.iter().enumerate().map(|(o, option)| {
                    let picked = self.picks[q].contains(&o);
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            toggle_button(
                                SharedString::from(format!("permission-option-{q}-{o}")),
                                option.label.clone(),
                                picked,
                                cx,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(q, o, cx))),
                        )
                        .when(!option.description.is_empty(), |column| {
                            column.child(
                                div()
                                    .pl_2()
                                    .text_xs()
                                    .text_color(colors.text_muted)
                                    .child(option.description.clone()),
                            )
                        })
                });
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .when(!question.header.is_empty(), |column| {
                        column.child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.text_muted)
                                .child(question.header.to_uppercase()),
                        )
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_color(colors.text)
                            .child(question.question.clone()),
                    )
                    .when(question.multi_select, |column| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(colors.text_muted)
                                .child("Pick any that apply."),
                        )
                    })
                    .child(div().flex().flex_col().gap_1().children(options))
                    .child(self.others[q].clone())
            });
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(questions)
            .into_any_element()
    }

    fn render_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        let later = button("permission-later", "Later", cx)
            .on_click(cx.listener(|_, _, _, cx| cx.emit(PermissionDialogEvent::Later)))
            .child(kbd("Esc", cx));
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(later)
            .child(div().flex_1());
        if self.request.is_question() {
            let answers = self.answers(cx);
            row = row
                .child(
                    button("permission-decline", "Decline", cx)
                        .on_click(cx.listener(|this, _, _, cx| this.deny(cx))),
                )
                .child(match answers {
                    Some(answers) => primary_button("permission-answer", "Answer", cx)
                        .h(px(24.0))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.answer(PermissionDecision::Answer(answers.clone()), cx)
                        }))
                        .into_any_element(),
                    None => disabled_button("permission-answer", "Answer", cx).into_any_element(),
                });
        } else {
            row = row
                .child(
                    button("permission-deny", "Deny", cx)
                        .on_click(cx.listener(|this, _, _, cx| this.deny(cx))),
                )
                .when(self.request.can_allow_for_run(), |row| {
                    row.child(
                        button("permission-allow-run", "Allow for this run", cx).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.answer(PermissionDecision::AllowForRun, cx)
                            }),
                        ),
                    )
                })
                .child(
                    primary_button("permission-allow", "Allow", cx)
                        .h(px(24.0))
                        .on_click(
                            cx.listener(|this, _, _, cx| {
                                this.answer(PermissionDecision::Allow, cx)
                            }),
                        ),
                );
        }
        row.into_any_element()
    }
}

impl Render for PermissionDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let question = self.request.is_question();
        let title = if question {
            "Claude has a question for you".to_owned()
        } else {
            format!("Claude wants to use {}", self.request.display_name)
        };
        let position = (self.waiting > 1).then(|| format!("1 of {}", self.waiting));
        let body = if question {
            self.render_questions(cx)
        } else {
            self.render_tool(cx)
        };
        let buttons = self.render_buttons(cx);
        div()
            .id("permission-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(72.0))
            .bg(colors.overlay)
            .occlude()
            .child(
                div()
                    .id("permission-dialog")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w(px(640.0))
                    .max_h(px(680.0))
                    .overflow_y_scroll()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.claude.opacity(0.6))
                    .bg(colors.elevated_surface)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .child(div().text_color(colors.claude).child("?"))
                            .child(div().flex_1().child(title))
                            .children(position.map(|position| {
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::NORMAL)
                                    .text_color(colors.text_muted)
                                    .child(position)
                            })),
                    )
                    .children(self.context.clone().map(|context| {
                        div()
                            .text_sm()
                            .text_color(colors.text_muted)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(context)
                    }))
                    .child(body)
                    .child(buttons),
            )
    }
}
