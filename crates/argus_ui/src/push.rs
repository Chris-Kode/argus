//! Push in the UI (spec §5.4): the progress popover under the title bar's
//! Push button and the dialogs only the user can answer.
//!
//! [`PushFlow`] hands each Push to the run manager
//! ([`argus_runs::RunManager::push`]), which queues it behind the
//! repository's accepts, and follows its ticket. Every git command runs on
//! the tokio runtime in the integration worktree; this entity only shows:
//!
//! - the progress popover: each step, every git command with its output,
//!   and the result. Errors keep the output visible, and "Copy output" puts
//!   it on the clipboard;
//! - the "No upstream" confirmation, which pushes with `--set-upstream` to
//!   a remote picked from the list;
//! - the one-time notice when the rebase moves a head branch checked out in
//!   one of the user's checkouts (shared with Accept);
//! - the conflict dialog of a pull-before-push rebase that conflicts.
//!
//! Dropping the entity (or Cancel) drops the ticket, which stops the Push at
//! its next step.

use std::path::{Path, PathBuf};

use argus_git::push::StepLog;
use argus_runs::{
    PushConflict, PushError, PushEvent, PushMode, PushOutcome, PushReport, PushRequest, PushStep,
    PushTicket,
};
use gpui::{
    AnyElement, ClipboardItem, Context, EventEmitter, FontWeight, MouseButton, SharedString, Task,
    Window, div, prelude::*, px,
};
use tokio::sync::oneshot;

use crate::AppContext;
use crate::components::{button, disabled_button, primary_button, toggle_button};
use crate::runtime::spawn_job;
use crate::theme::ActiveTheme;

/// Where the popover hangs, below the title bar.
const POPOVER_TOP: f32 = 40.0;

/// What the push flow tells the workspace.
#[derive(Clone, Debug)]
pub enum PushFlowEvent {
    /// A Push ended (however it ended): refresh the branches, so the diff,
    /// the comment anchors and the Push counts follow.
    Finished,
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

/// Where the Push shown in the popover is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    /// Running this step.
    Running(PushStep),
    /// Finished; the summary.
    Done(String),
    /// The rebase conflicts.
    Conflict,
    /// Failed; the error with git's output.
    Failed(String),
    /// Cancelled by the user.
    Cancelled,
}

/// The Push shown in the popover.
struct Progress {
    head: String,
    steps: Vec<PushStep>,
    logs: Vec<StepLog>,
    status: Status,
}

impl Progress {
    /// The popover's content as plain text, for the clipboard.
    fn text(&self) -> String {
        let mut text = String::new();
        for log in &self.logs {
            text.push_str("$ ");
            text.push_str(&log.command);
            text.push('\n');
            if !log.output.is_empty() {
                text.push_str(&log.output);
                text.push('\n');
            }
        }
        match &self.status {
            Status::Failed(error) => {
                text.push_str("\nerror: ");
                text.push_str(error);
                text.push('\n');
            }
            Status::Done(summary) => {
                text.push('\n');
                text.push_str(summary);
                text.push('\n');
            }
            _ => {}
        }
        text
    }
}

/// A dialog waiting for the user.
enum Dialog {
    /// The head has no upstream: push with `--set-upstream`?
    SetUpstream {
        request: PushRequest,
        remotes: Vec<String>,
        selected: usize,
    },
    /// The rebase will move a branch checked out in the user's checkout.
    CheckedOut {
        head: String,
        path: PathBuf,
        reply: oneshot::Sender<bool>,
    },
    /// The pull-before-push rebase conflicts.
    Conflict(PushConflict),
}

/// The Push of the window, its popover and its dialogs.
pub struct PushFlow {
    task: Option<Task<()>>,
    progress: Option<Progress>,
    popover_open: bool,
    dialog: Option<Dialog>,
}

impl EventEmitter<PushFlowEvent> for PushFlow {}

impl PushFlow {
    /// No Push in flight.
    pub fn new(_: &mut Context<Self>) -> Self {
        Self {
            task: None,
            progress: None,
            popover_open: false,
            dialog: None,
        }
    }

    /// Whether a Push is queued or running.
    pub fn is_running(&self) -> bool {
        self.task.is_some()
    }

    /// Whether a dialog is shown.
    pub fn has_dialog(&self) -> bool {
        self.dialog.is_some()
    }

    /// Whether anything (popover or dialog) is shown.
    pub fn is_visible(&self) -> bool {
        self.dialog.is_some() || (self.popover_open && self.progress.is_some())
    }

    /// Shows or hides the progress popover of the current or last Push.
    pub fn toggle_popover(&mut self, cx: &mut Context<Self>) {
        self.popover_open = !self.popover_open && self.progress.is_some();
        cx.notify();
    }

    /// Escape: answers "no" to the dialog, or closes the popover. `false`
    /// when nothing was shown.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        if self.dialog.is_some() {
            self.close_dialog(false, cx);
            return true;
        }
        if self.popover_open {
            self.popover_open = false;
            cx.notify();
            return true;
        }
        false
    }

    /// Asks before pushing a head without an upstream: `git push
    /// --set-upstream <remote> <head>`, the remote picked from `remotes`
    /// (`origin` preselected when present).
    pub fn offer_set_upstream(
        &mut self,
        request: PushRequest,
        remotes: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if self.is_running() || remotes.is_empty() {
            return;
        }
        let selected = remotes
            .iter()
            .position(|remote| remote == "origin")
            .unwrap_or(0);
        self.dialog = Some(Dialog::SetUpstream {
            request,
            remotes,
            selected,
        });
        cx.notify();
    }

    /// Queues `request` and follows it in the popover.
    pub fn push(&mut self, request: PushRequest, cx: &mut Context<Self>) {
        if self.is_running() {
            self.popover_open = true;
            cx.notify();
            return;
        }
        let head = request.head.clone();
        let runs = cx.global::<AppContext>().runs.clone();
        let submit = spawn_job(cx, async move { runs.push(request) });
        let task = cx.spawn(async move |this, cx| {
            let ticket = match submit.await {
                Ok(Ok(ticket)) => ticket,
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| this.finished(Err(error), cx))
                        .ok();
                    return;
                }
                Err(error) => {
                    tracing::warn!(%error, "push submission task failed");
                    this.update(cx, |this, cx| {
                        this.finished(Err(PushError::QueueClosed), cx)
                    })
                    .ok();
                    return;
                }
            };
            let PushTicket { mut events, result } = ticket;
            while let Some(event) = events.recv().await {
                if this.update(cx, |this, cx| this.event(event, cx)).is_err() {
                    return;
                }
            }
            let outcome = result.await.unwrap_or(Err(PushError::QueueClosed));
            this.update(cx, |this, cx| this.finished(outcome, cx)).ok();
        });
        self.task = Some(task);
        self.progress = Some(Progress {
            head,
            steps: vec![PushStep::Queued],
            logs: Vec::new(),
            status: Status::Running(PushStep::Queued),
        });
        self.popover_open = true;
        cx.notify();
    }

    /// Stops following the Push; it stops at its next step (a git command
    /// already running finishes first).
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.task.take().is_none() {
            return;
        }
        if let Some(Dialog::CheckedOut { .. }) = self.dialog {
            self.dialog = None;
        }
        if let Some(progress) = self.progress.as_mut() {
            progress.status = Status::Cancelled;
        }
        cx.emit(PushFlowEvent::Notice {
            message: "Push cancelled; nothing more is pushed.".into(),
            is_error: false,
        });
        cx.emit(PushFlowEvent::Finished);
        cx.notify();
    }

    fn event(&mut self, event: PushEvent, cx: &mut Context<Self>) {
        match event {
            PushEvent::Step(step) => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.steps.push(step);
                    progress.status = Status::Running(step);
                }
            }
            PushEvent::Log(log) => {
                if let Some(progress) = self.progress.as_mut() {
                    progress.logs.push(log);
                }
            }
            PushEvent::CheckedOut { head, path, reply } => {
                self.dialog = Some(Dialog::CheckedOut { head, path, reply });
            }
        }
        cx.notify();
    }

    fn finished(&mut self, outcome: Result<PushOutcome, PushError>, cx: &mut Context<Self>) {
        self.task = None;
        if matches!(self.dialog, Some(Dialog::CheckedOut { .. })) {
            self.dialog = None;
        }
        let (status, notice, is_error) = match outcome {
            Ok(PushOutcome::Pushed(report)) => {
                let summary = pushed_summary(&report);
                (Status::Done(summary.clone()), summary, false)
            }
            Ok(PushOutcome::UpToDate(report)) => {
                let summary = match report.rebased {
                    Some(_) => format!(
                        "Pulled `{}` into `{}`; nothing left to push.",
                        report.upstream, report.head
                    ),
                    None => format!(
                        "Nothing to push: `{}` is up to date with `{}`.",
                        report.head, report.upstream
                    ),
                };
                (Status::Done(summary.clone()), summary, false)
            }
            Ok(PushOutcome::Conflict(conflict)) => {
                let notice = format!(
                    "`{}` does not rebase cleanly on `{}`; nothing was pushed.",
                    conflict.head, conflict.upstream
                );
                self.dialog = Some(Dialog::Conflict(conflict));
                (Status::Conflict, notice, true)
            }
            Ok(PushOutcome::Cancelled) => (
                Status::Cancelled,
                "Push cancelled; nothing more is pushed.".to_owned(),
                false,
            ),
            Err(error) => {
                let message = error.to_string();
                let first = message.lines().next().unwrap_or_default().to_owned();
                // The popover keeps the full output; the status bar gets
                // one line.
                self.popover_open = true;
                (
                    Status::Failed(message),
                    format!("Push failed: {first}"),
                    true,
                )
            }
        };
        if let Some(progress) = self.progress.as_mut() {
            progress.status = status;
        }
        cx.emit(PushFlowEvent::Notice {
            message: notice.into(),
            is_error,
        });
        cx.emit(PushFlowEvent::Finished);
        cx.notify();
    }

    /// Answers the dialog: confirm (Push / Continue / Close) or cancel.
    fn close_dialog(&mut self, confirm: bool, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            Dialog::SetUpstream {
                mut request,
                remotes,
                selected,
            } => {
                if confirm && let Some(remote) = remotes.get(selected) {
                    request.mode = PushMode::SetUpstream {
                        remote: remote.clone(),
                    };
                    self.push(request, cx);
                }
            }
            Dialog::CheckedOut { reply, .. } => {
                let _ = reply.send(confirm);
            }
            Dialog::Conflict(_) => {}
        }
        cx.emit(PushFlowEvent::DialogsClosed);
        cx.notify();
    }

    fn select_remote(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(Dialog::SetUpstream {
            selected, remotes, ..
        }) = self.dialog.as_mut()
            && index < remotes.len()
        {
            *selected = index;
            cx.notify();
        }
    }

    fn copy_output(&mut self, cx: &mut Context<Self>) {
        if let Some(progress) = &self.progress {
            cx.write_to_clipboard(ClipboardItem::new_string(progress.text()));
            cx.emit(PushFlowEvent::Notice {
                message: "Copied the push output.".into(),
                is_error: false,
            });
        }
    }

    // ---- rendering ----------------------------------------------------------

    fn render_popover(&self, progress: &Progress, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let running = matches!(progress.status, Status::Running(_));
        let failed = matches!(progress.status, Status::Failed(_));
        let last = progress.steps.len().saturating_sub(1);
        let steps = progress.steps.iter().enumerate().map(|(index, step)| {
            let (mark, color) = if index < last {
                ("✓", colors.diff_added_text)
            } else {
                match &progress.status {
                    Status::Running(_) => ("…", colors.accent),
                    Status::Done(_) => ("✓", colors.diff_added_text),
                    Status::Failed(_) | Status::Conflict => ("✗", colors.error),
                    Status::Cancelled => ("–", colors.text_muted),
                }
            };
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child(div().w(px(12.0)).text_color(color).child(mark))
                .child(div().text_color(colors.text).child(step.label()))
        });
        let output =
            (!progress.logs.is_empty()).then(|| {
                div()
                    .id("push-output")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_h(px(240.0))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.background)
                    .font_family(theme.fonts.code_family.clone())
                    .text_xs()
                    .children(progress.logs.iter().map(|log| {
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_color(if log.success {
                                        colors.text
                                    } else {
                                        colors.error
                                    })
                                    .child(format!("$ {}", log.command)),
                            )
                            .children(log.output.lines().map(|line| {
                                div().text_color(colors.text_muted).child(line.to_owned())
                            }))
                    }))
            });
        let result = match &progress.status {
            Status::Done(summary) => Some(
                div()
                    .text_sm()
                    .text_color(colors.diff_added_text)
                    .child(summary.clone())
                    .into_any_element(),
            ),
            Status::Failed(error) => Some(
                div()
                    .id("push-error")
                    .max_h(px(160.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .text_color(colors.error)
                    .children(error.lines().map(|line| div().child(line.to_owned())))
                    .into_any_element(),
            ),
            Status::Conflict => Some(
                div()
                    .text_sm()
                    .text_color(colors.error)
                    .child("The rebase conflicts; it was aborted and nothing was pushed.")
                    .into_any_element(),
            ),
            Status::Cancelled => Some(
                div()
                    .text_sm()
                    .text_color(colors.text_muted)
                    .child("Cancelled.")
                    .into_any_element(),
            ),
            Status::Running(_) => None,
        };
        let mut actions: Vec<AnyElement> = Vec::new();
        if !progress.logs.is_empty() || failed {
            actions.push(
                button("push-copy-output", "Copy output", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.copy_output(cx)))
                    .into_any_element(),
            );
        }
        if running {
            actions.push(
                button("push-cancel", "Cancel", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
                    .into_any_element(),
            );
        } else {
            actions.push(
                button("push-close", "Close", cx)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.popover_open = false;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        div()
            .id("push-popover")
            .absolute()
            .top(px(POPOVER_TOP))
            .right(px(8.0))
            .w(px(480.0))
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface)
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child(format!("Push `{}`", progress.head)),
            )
            .child(div().flex().flex_col().gap_0p5().children(steps))
            .children(output)
            .children(result)
            .child(div().flex().justify_end().gap_2().children(actions))
            .into_any_element()
    }

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
            .id("push-backdrop")
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
                    .w(px(560.0))
                    .max_h(px(600.0))
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

    fn render_set_upstream(
        &self,
        head: &str,
        remotes: &[String],
        selected: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let remote = remotes.get(selected).cloned().unwrap_or_default();
        let code = |text: String| {
            div()
                .px_1()
                .rounded_sm()
                .bg(colors.element_hover)
                .font_family(theme.fonts.code_family.clone())
                .child(text)
        };
        let mut content = vec![
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .text_sm()
                .text_color(colors.text_muted)
                .child(code(head.to_owned()))
                .child(
                    "has no upstream yet. Argus can push it and record the remote branch as its \
                     upstream, so later pushes and the Push counts follow it.",
                )
                .into_any_element(),
        ];
        if remotes.len() > 1 {
            content.push(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .text_sm()
                    .child(div().text_color(colors.text_muted).child("Remote:"))
                    .children(remotes.iter().enumerate().map(|(index, name)| {
                        toggle_button(("push-remote", index), name.clone(), index == selected, cx)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.select_remote(index, cx)),
                            )
                    }))
                    .into_any_element(),
            );
        }
        content.push(
            div()
                .text_xs()
                .text_color(colors.text_muted)
                .child(code(format!("git push -u {remote} {head}")))
                .into_any_element(),
        );
        let actions = vec![
            button("push-upstream-cancel", "Cancel", cx)
                .on_click(cx.listener(|this, _, _, cx| this.close_dialog(false, cx)))
                .into_any_element(),
            primary_button("push-upstream-confirm", "Push", cx)
                .on_click(cx.listener(|this, _, _, cx| this.close_dialog(true, cx)))
                .into_any_element(),
        ];
        self.frame(
            "push-upstream-dialog",
            format!("Push `{head}` and set its upstream?").into(),
            content,
            actions,
            cx,
        )
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
                "The remote is ahead, so Argus will rebase the branch on it before pushing. \
                 Your checkout will then show the difference as changes until you run:",
            ))
            .child(row().child(code(format!("git reset --keep {head}"))))
            .child(row().child("Argus never runs anything in that checkout."))
            .into_any_element();
        let note = div()
            .text_xs()
            .text_color(colors.text_placeholder)
            .child("Continue is remembered for this repository (Accept shares it).")
            .into_any_element();
        let actions = vec![
            button("push-checkout-cancel", "Cancel", cx)
                .on_click(cx.listener(|this, _, _, cx| this.close_dialog(false, cx)))
                .into_any_element(),
            primary_button("push-checkout-continue", "Continue", cx)
                .on_click(cx.listener(|this, _, _, cx| this.close_dialog(true, cx)))
                .into_any_element(),
        ];
        self.frame(
            "push-checkout-dialog",
            format!("`{head}` is checked out").into(),
            vec![text, note],
            actions,
            cx,
        )
    }

    fn render_conflict(&self, conflict: &PushConflict, cx: &mut Context<Self>) -> AnyElement {
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
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(format!("OPERATION: {}", conflict.operation.to_uppercase()))
                .into_any_element(),
            div()
                .text_sm()
                .text_color(colors.text_muted)
                .child(format!(
                    "`{}` is behind `{}` ({}), and its commits do not rebase cleanly on the \
                     remote's. The rebase was aborted: `{}` did not move ({}) and nothing was \
                     pushed.",
                    conflict.upstream,
                    conflict.upstream,
                    conflict.upstream_tip.short(8),
                    conflict.head,
                    conflict.head_tip.short(8),
                ))
                .into_any_element(),
            files.into_any_element(),
            div()
                .text_xs()
                .text_color(colors.text_placeholder)
                .child(
                    "Resolve it in your own checkout (git pull --rebase), then Push again. \
                     Resolving with Claude from here is coming next.",
                )
                .into_any_element(),
        ];
        let actions = vec![
            disabled_button("push-conflict-resolve", "Resolve with Claude", cx).into_any_element(),
            primary_button("push-conflict-close", "Close", cx)
                .on_click(cx.listener(|this, _, _, cx| this.close_dialog(false, cx)))
                .into_any_element(),
        ];
        self.frame(
            "push-conflict-dialog",
            "Pull before push conflicts".into(),
            content,
            actions,
            cx,
        )
    }
}

/// `Pushed feature to origin/feature (abc12345)`, with the rebase and retry
/// notes.
fn pushed_summary(report: &PushReport) -> String {
    let mut summary = format!(
        "Pushed `{}` to `{}` ({})",
        report.head,
        report.upstream,
        report.commit.short(8)
    );
    if report.rebased.is_some() {
        summary.push_str(", after rebasing on the remote's commits");
    }
    if report.retried {
        summary.push_str("; the remote moved once and Argus fetched again");
    }
    if report.set_upstream {
        summary.push_str(&format!("; `{}` is now its upstream", report.upstream));
    }
    summary.push('.');
    summary
}

impl Render for PushFlow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The root is absolutely positioned: GPUI places it in the workspace
        // root, the dialog backdrop over everything, the popover below the
        // title bar.
        match &self.dialog {
            Some(Dialog::SetUpstream {
                request,
                remotes,
                selected,
            }) => {
                let head = request.head.clone();
                let remotes = remotes.clone();
                let selected = *selected;
                return self.render_set_upstream(&head, &remotes, selected, cx);
            }
            Some(Dialog::CheckedOut { head, path, .. }) => {
                let (head, path) = (head.clone(), path.clone());
                return self.render_checked_out(&head, &path, cx);
            }
            Some(Dialog::Conflict(conflict)) => {
                let conflict = conflict.clone();
                return self.render_conflict(&conflict, cx);
            }
            None => {}
        }
        match (&self.progress, self.popover_open) {
            (Some(progress), true) => self.render_popover(progress, cx),
            _ => div().into_any_element(),
        }
    }
}
