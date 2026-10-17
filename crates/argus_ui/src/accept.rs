//! Accept in the UI (spec §5.3, §6.4): the "Accepting…" state of queued
//! accepts and the dialogs only the user can answer.
//!
//! [`AcceptFlow`] hands each Accept to the run manager's per-repository
//! queue ([`argus_runs::RunManager::accept`]) and follows its ticket. All git
//! work, the Haiku call and the SQLite writes run on the tokio runtime; this
//! entity only shows progress and answers questions:
//!
//! - the one-time notice when the head branch is checked out in one of the
//!   user's checkouts (Continue is remembered per repository);
//! - the commit dialog: subject and body (editable), the `Argus-Comment`
//!   trailer, and the output of a failed `git commit` (hook) to retry;
//! - the conflict dialog listing the conflicted files.
//!
//! Questions of later accepts wait their turn: the queue only asks for the
//! Accept it is working on. Dropping the entity cancels every pending Accept.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use argus_runs::{
    AcceptConflict, AcceptError, AcceptEvent, AcceptOutcome, AcceptStage, AcceptTicket, Accepted,
    CommitDraft, CommitInput,
};
use argus_store::RunId;
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, MouseButton, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use tokio::sync::oneshot;

use crate::AppContext;
use crate::components::{button, disabled_button, kbd, primary_button};
use crate::runtime::spawn_job;
use crate::text_editor::{TextEditor, TextEditorEvent};
use crate::theme::ActiveTheme;

/// What the accept flow tells the workspace.
#[derive(Clone, Debug)]
pub enum AcceptFlowEvent {
    /// The run's Accept moved to `stage`; `None` once it ended.
    Stage {
        /// The run.
        run_id: RunId,
        /// Where it is.
        stage: Option<AcceptStage>,
    },
    /// The commit landed on the head branch: refresh the diff, re-anchor
    /// the comments, update the Push counts and close the proposal.
    Committed(Accepted),
    /// The last dialog closed; the focus goes back to the shell.
    DialogsClosed,
    /// A message for the status bar.
    Notice {
        /// Text to show.
        message: SharedString,
        /// Error (red) rather than information.
        is_error: bool,
    },
}

/// A dialog waiting for the user.
enum Dialog {
    /// The head branch is checked out in the user's checkout.
    CheckedOut {
        run_id: RunId,
        head: String,
        path: PathBuf,
        reply: oneshot::Sender<bool>,
    },
    /// The commit message editor.
    Commit(Box<CommitDialog>),
    /// The patch conflicts with the head tip.
    Conflict(AcceptConflict),
}

impl Dialog {
    fn run_id(&self) -> RunId {
        match self {
            Self::CheckedOut { run_id, .. } => *run_id,
            Self::Commit(dialog) => dialog.run_id,
            Self::Conflict(conflict) => conflict.run_id,
        }
    }
}

struct CommitDialog {
    run_id: RunId,
    draft: CommitDraft,
    subject: Entity<TextEditor>,
    body: Entity<TextEditor>,
    reply: oneshot::Sender<Option<CommitInput>>,
    empty_subject: bool,
    _subscriptions: [Subscription; 2],
}

/// One Accept in flight.
struct Job {
    stage: AcceptStage,
    _task: Task<()>,
}

/// The accepts of the window and their dialogs.
pub struct AcceptFlow {
    jobs: HashMap<RunId, Job>,
    dialogs: VecDeque<Dialog>,
}

impl EventEmitter<AcceptFlowEvent> for AcceptFlow {}

impl AcceptFlow {
    /// No accept in flight.
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            jobs: HashMap::new(),
            dialogs: VecDeque::new(),
        }
    }

    /// Where the run's Accept is, if it is queued or in progress.
    pub fn stage(&self, run_id: RunId) -> Option<AcceptStage> {
        self.jobs.get(&run_id).map(|job| job.stage)
    }

    /// Whether a dialog is shown.
    pub fn has_dialog(&self) -> bool {
        !self.dialogs.is_empty()
    }

    /// Queues the Accept of `run_id` and follows it.
    pub fn accept(&mut self, run_id: RunId, cx: &mut Context<Self>) {
        if self.jobs.contains_key(&run_id) {
            self.notice("This proposal is already being accepted.", false, cx);
            return;
        }
        let runs = cx.global::<AppContext>().runs.clone();
        let submit = spawn_job(cx, async move { runs.accept(run_id).await });
        let task = cx.spawn(async move |this, cx| {
            let ticket = match submit.await {
                Ok(Ok(ticket)) => ticket,
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| this.finished(run_id, Err(error), cx))
                        .ok();
                    return;
                }
                Err(error) => {
                    tracing::warn!(%error, "accept submission task failed");
                    this.update(cx, |this, cx| {
                        this.finished(run_id, Err(AcceptError::QueueClosed), cx);
                    })
                    .ok();
                    return;
                }
            };
            let AcceptTicket {
                mut events, result, ..
            } = ticket;
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.event(run_id, event, cx))
                    .is_err()
                {
                    return;
                }
            }
            let outcome = result.await.unwrap_or(Err(AcceptError::QueueClosed));
            this.update(cx, |this, cx| this.finished(run_id, outcome, cx))
                .ok();
        });
        self.jobs.insert(
            run_id,
            Job {
                stage: AcceptStage::Queued,
                _task: task,
            },
        );
        cx.emit(AcceptFlowEvent::Stage {
            run_id,
            stage: Some(AcceptStage::Queued),
        });
        cx.notify();
    }

    /// Escape: cancels the dialog on top. `false` when none is shown.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dialogs.is_empty() {
            return false;
        }
        self.answer_front(false, cx);
        true
    }

    fn notice(&self, message: impl Into<SharedString>, is_error: bool, cx: &mut Context<Self>) {
        cx.emit(AcceptFlowEvent::Notice {
            message: message.into(),
            is_error,
        });
    }

    fn event(&mut self, run_id: RunId, event: AcceptEvent, cx: &mut Context<Self>) {
        match event {
            AcceptEvent::Stage(stage) => {
                if let Some(job) = self.jobs.get_mut(&run_id) {
                    job.stage = stage;
                }
                cx.emit(AcceptFlowEvent::Stage {
                    run_id,
                    stage: Some(stage),
                });
            }
            AcceptEvent::CheckedOut { head, path, reply } => {
                self.dialogs.push_back(Dialog::CheckedOut {
                    run_id,
                    head,
                    path,
                    reply,
                });
            }
            AcceptEvent::CommitMessage { draft, reply } => {
                let subject = cx.new(|cx| TextEditor::new(draft.subject.clone(), "Subject", cx));
                let body = cx.new(|cx| {
                    TextEditor::new(
                        draft.body.clone(),
                        "Commit body (optional): what changed and why",
                        cx,
                    )
                });
                let on_edit =
                    |this: &mut Self, _, event: &TextEditorEvent, cx: &mut Context<Self>| {
                        match event {
                            TextEditorEvent::Submit => this.answer_front(true, cx),
                            TextEditorEvent::Cancel => this.answer_front(false, cx),
                            TextEditorEvent::Edited => cx.notify(),
                        }
                    };
                let subscriptions = [
                    cx.subscribe(&subject, on_edit),
                    cx.subscribe(&body, on_edit),
                ];
                if self.dialogs.is_empty() {
                    subject.update(cx, |editor, cx| editor.request_focus(cx));
                }
                self.dialogs
                    .push_back(Dialog::Commit(Box::new(CommitDialog {
                        run_id,
                        draft,
                        subject,
                        body,
                        reply,
                        empty_subject: false,
                        _subscriptions: subscriptions,
                    })));
            }
        }
        cx.notify();
    }

    fn finished(
        &mut self,
        run_id: RunId,
        outcome: Result<AcceptOutcome, AcceptError>,
        cx: &mut Context<Self>,
    ) {
        self.jobs.remove(&run_id);
        let had_dialogs = !self.dialogs.is_empty();
        // A question the queue stopped waiting for is moot.
        self.dialogs
            .retain(|dialog| matches!(dialog, Dialog::Conflict(_)) || dialog.run_id() != run_id);
        match outcome {
            Ok(AcceptOutcome::Committed(accepted)) => {
                for warning in &accepted.warnings {
                    tracing::warn!(%warning, "accept finished with a warning");
                }
                cx.emit(AcceptFlowEvent::Committed(accepted));
            }
            Ok(AcceptOutcome::Conflict(conflict)) => {
                self.notice(
                    format!(
                        "The proposal conflicts with `{}`; nothing was committed.",
                        conflict.head
                    ),
                    true,
                    cx,
                );
                self.dialogs.push_back(Dialog::Conflict(conflict));
            }
            Ok(AcceptOutcome::Cancelled) => {
                self.notice("Accept cancelled; the proposal stays open.", false, cx);
            }
            Err(error) => {
                tracing::warn!(%error, "accept failed");
                self.notice(format!("Accept failed: {error}"), true, cx);
            }
        }
        // After the outcome: a committed proposal's tab is closed by then,
        // so it does not go back to following its deleted worktree.
        cx.emit(AcceptFlowEvent::Stage {
            run_id,
            stage: None,
        });
        // Only move the focus when a dialog had it.
        if had_dialogs {
            self.focus_front(cx);
        }
        cx.notify();
    }

    /// Answers the dialog on top: confirm (Continue / Commit / Close) or
    /// cancel.
    fn answer_front(&mut self, confirm: bool, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialogs.pop_front() else {
            return;
        };
        match dialog {
            Dialog::CheckedOut { reply, .. } => {
                let _ = reply.send(confirm);
            }
            Dialog::Commit(mut dialog) => {
                if !confirm {
                    let _ = dialog.reply.send(None);
                } else {
                    let subject = dialog.subject.read(cx).text().trim().to_owned();
                    if subject.is_empty() {
                        dialog.empty_subject = true;
                        dialog
                            .subject
                            .update(cx, |editor, cx| editor.request_focus(cx));
                        self.dialogs.push_front(Dialog::Commit(dialog));
                        cx.notify();
                        return;
                    }
                    let body = dialog.body.read(cx).text().to_owned();
                    let _ = dialog.reply.send(Some(CommitInput { subject, body }));
                }
            }
            Dialog::Conflict(_) => {}
        }
        self.focus_front(cx);
        cx.notify();
    }

    fn focus_front(&mut self, cx: &mut Context<Self>) {
        match self.dialogs.front() {
            Some(Dialog::Commit(dialog)) => {
                dialog
                    .subject
                    .update(cx, |editor, cx| editor.request_focus(cx));
            }
            Some(_) => {}
            None => cx.emit(AcceptFlowEvent::DialogsClosed),
        }
    }

    // ---- rendering ----------------------------------------------------------

    fn frame(
        &self,
        id: &'static str,
        title: SharedString,
        content: Vec<AnyElement>,
        actions: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        div()
            .id("accept-backdrop")
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
                    .id(id)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w(px(620.0))
                    .max_h(px(640.0))
                    .overflow_y_scroll()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .child(title),
                    )
                    .children(content)
                    .child(div().flex().justify_end().gap_2().children(actions)),
            )
            .into_any_element()
    }

    fn render_checked_out(&self, head: &str, path: &Path, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let code = |text: String| {
            div()
                .px_1()
                .rounded_sm()
                .bg(colors.element_hover)
                .font_family(theme.fonts.code_family.clone())
                .child(text)
        };
        let row = || {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .text_sm()
                .text_color(colors.text_muted)
        };
        let text = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                row()
                    .child(code(head.to_owned()))
                    .child("is checked out at")
                    .child(code(crate::workspace::display_path(path))),
            )
            .child(row().child(
                "Argus will move the branch; your checkout will show the new commits as reverse \
                 changes until you run:",
            ))
            .child(row().child(code(format!("git reset --keep {head}"))))
            .child(row().child("Argus never runs anything in that checkout."))
            .into_any_element();
        let note = div()
            .text_xs()
            .text_color(colors.text_placeholder)
            .child("Continue is remembered for this repository.")
            .into_any_element();
        let actions = vec![
            button("accept-checkout-cancel", "Cancel", cx)
                .on_click(cx.listener(|this, _, _, cx| this.answer_front(false, cx)))
                .into_any_element(),
            primary_button("accept-checkout-continue", "Continue", cx)
                .on_click(cx.listener(|this, _, _, cx| this.answer_front(true, cx)))
                .into_any_element(),
        ];
        self.frame(
            "accept-checkout-dialog",
            format!("`{head}` is checked out").into(),
            vec![text, note],
            actions,
            cx,
        )
    }

    fn render_commit(&self, dialog: &CommitDialog, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let draft = &dialog.draft;
        let label = |text: &'static str| {
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(text)
        };
        let field = |editor: Entity<TextEditor>, invalid: bool, min_height: f32| {
            div()
                .min_h(px(min_height))
                .max_h(px(260.0))
                .overflow_hidden()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if invalid { colors.error } else { colors.border })
                .bg(colors.background)
                .font_family(theme.fonts.code_family.clone())
                .text_sm()
                .child(editor)
        };
        let files = draft.files.len();
        let mut content = vec![
            div()
                .text_xs()
                .text_color(colors.text_muted)
                .child(format!(
                    "One local commit on `{}` ({files} file{}). Nothing is pushed.",
                    draft.head,
                    if files == 1 { "" } else { "s" }
                ))
                .into_any_element(),
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(label("SUBJECT"))
                .child(field(dialog.subject.clone(), dialog.empty_subject, 0.0))
                .when(dialog.empty_subject, |column| {
                    column.child(
                        div()
                            .text_xs()
                            .text_color(colors.error)
                            .child("Enter a subject."),
                    )
                })
                .into_any_element(),
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(label("BODY"))
                .child(field(dialog.body.clone(), false, 96.0))
                .when_some(draft.body_note.clone(), |column, note| {
                    column.child(div().text_xs().text_color(colors.warning).child(format!(
                        "{note}; write the body yourself or leave it empty."
                    )))
                })
                .into_any_element(),
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(label("TRAILER"))
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.text_muted)
                        .font_family(theme.fonts.code_family.clone())
                        .child(draft.trailer.clone()),
                )
                .into_any_element(),
        ];
        if let Some(error) = &draft.commit_error {
            content.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.error.opacity(0.6))
                    .bg(colors.error.opacity(0.08))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.error)
                            .child("git commit failed (hook output below). Fix it, then retry."),
                    )
                    .child(
                        div()
                            .id("accept-commit-error")
                            .max_h(px(160.0))
                            .overflow_y_scroll()
                            .text_xs()
                            .font_family(theme.fonts.code_family.clone())
                            .text_color(colors.text)
                            .children(
                                error
                                    .lines()
                                    .map(|line| div().child(line.to_owned()))
                                    .collect::<Vec<_>>(),
                            ),
                    )
                    .into_any_element(),
            );
        }
        let commit_label = if draft.commit_error.is_some() {
            "Retry commit"
        } else {
            "Commit"
        };
        let actions = vec![
            button("accept-commit-cancel", "Cancel", cx)
                .on_click(cx.listener(|this, _, _, cx| this.answer_front(false, cx)))
                .child(kbd("esc", cx))
                .into_any_element(),
            primary_button("accept-commit-confirm", commit_label, cx)
                .h(px(24.0))
                .gap_1()
                .on_click(cx.listener(|this, _, _, cx| this.answer_front(true, cx)))
                .into_any_element(),
        ];
        self.frame(
            "accept-commit-dialog",
            "Commit the accepted proposal".into(),
            content,
            actions,
            cx,
        )
    }

    fn render_conflict(&self, conflict: &AcceptConflict, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let files = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .font_family(theme.fonts.code_family.clone())
            .text_sm()
            .children(conflict.files.iter().map(|file| {
                div()
                    .text_color(colors.warning)
                    .child(format!("U  {}", file.display()))
            }));
        let content = vec![
            div()
                .text_sm()
                .text_color(colors.text_muted)
                .child(format!(
                    "The proposal does not apply cleanly on the tip of `{}` ({}). Nothing was \
                     committed, the head branch did not move, and the proposal stays open.",
                    conflict.head,
                    conflict.head_tip.short(8)
                ))
                .into_any_element(),
            files.into_any_element(),
            div()
                .text_xs()
                .text_color(colors.text_placeholder)
                .child(
                    "Reject the proposal and run the comment again on the new code, or reply to \
                     it. Resolving with Claude from here is coming next.",
                )
                .into_any_element(),
        ];
        let actions = vec![
            disabled_button("accept-conflict-resolve", "Resolve with Claude", cx)
                .into_any_element(),
            primary_button("accept-conflict-close", "Close", cx)
                .on_click(cx.listener(|this, _, _, cx| this.answer_front(false, cx)))
                .into_any_element(),
        ];
        self.frame(
            "accept-conflict-dialog",
            "Accept conflicts".into(),
            content,
            actions,
            cx,
        )
    }
}

impl Render for AcceptFlow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The backdrop is the root: GPUI positions an absolute element in
        // its parent, here the workspace root.
        match self.dialogs.front() {
            None => div().into_any_element(),
            Some(Dialog::CheckedOut { head, path, .. }) => self.render_checked_out(head, path, cx),
            Some(Dialog::Commit(dialog)) => self.render_commit(dialog, cx),
            Some(Dialog::Conflict(conflict)) => self.render_conflict(conflict, cx),
        }
    }
}
