//! The conflict dialog (spec §6.6): an Accept or the pull before Push hit
//! a conflict, and nothing moved.
//!
//! [`ConflictDialog`] names the operation ("Accept of comment … conflicts"
//! / "Pull before push conflicts"), lists the conflicted files (a click
//! previews the conflicted content with its markers) and offers **Resolve
//! with Claude**, with the Run menu's model and effort, or **Cancel**.
//! Resolve starts a resolve run on the tokio runtime
//! ([`argus_runs::RunManager::resolve_accept`],
//! [`argus_runs::RunManager::resolve_push`]); its result comes back as a
//! proposal. Cancel leaves the head branch, the remote and the user's
//! checkout as they are; for a pull-before-push resolution in progress (the
//! rebase stopped again at a later commit) it also deletes that rebase's
//! worktree and abandons the Push.

use std::path::PathBuf;

use argus_runs::{AcceptConflict, PushConflict, PushRequest};
use argus_store::RunId;
use gpui::{
    AnyElement, Context, EventEmitter, FontWeight, MouseButton, SharedString, Task, Window, div,
    prelude::*, px,
};

use crate::AppContext;
use crate::components::{button, disabled_button, kbd, primary_button};
use crate::followup;
use crate::runtime::spawn_job;
use crate::theme::ActiveTheme;

/// Longest preview shown, in lines; the file in the resolve worktree has
/// the rest.
const PREVIEW_MAX_LINES: usize = 4000;

/// What conflicted.
#[derive(Clone, Debug)]
pub enum ConflictSource {
    /// The Accept of a comment's proposal.
    Accept(AcceptConflict),
    /// The rebase of a pull before push.
    Push {
        /// The Push it belongs to.
        request: Box<PushRequest>,
        /// The conflict.
        conflict: Box<PushConflict>,
    },
}

impl ConflictSource {
    fn files(&self) -> &[PathBuf] {
        match self {
            Self::Accept(conflict) => &conflict.files,
            Self::Push { conflict, .. } => &conflict.files,
        }
    }

    fn title(&self) -> String {
        match self {
            Self::Accept(conflict) => format!(
                "Accept of comment “{}” conflicts",
                excerpt(&conflict.comment_label, 60)
            ),
            Self::Push { conflict, .. } => match &conflict.resolve {
                Some(progress) => format!(
                    "Pull before push conflicts again (commit {})",
                    progress.resolved + 1
                ),
                None => "Pull before push conflicts".to_owned(),
            },
        }
    }
}

/// What the dialog tells the workspace.
#[derive(Clone, Debug)]
pub enum ConflictDialogEvent {
    /// A resolve run started; its proposal opens when it ends.
    ResolveStarted(RunId),
    /// The dialog closed.
    Closed,
    /// A message for the status bar.
    Notice {
        /// Text to show.
        message: SharedString,
        /// Error (red) rather than information.
        is_error: bool,
    },
}

/// The preview of one conflicted file.
enum Preview {
    Loading,
    Ready(Vec<SharedString>, bool),
    Failed(SharedString),
}

/// One conflict dialog.
pub struct ConflictDialog {
    source: ConflictSource,
    selected: Option<usize>,
    preview: Option<Preview>,
    starting: bool,
    cancelling: bool,
    error: Option<SharedString>,
    preview_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
}

impl EventEmitter<ConflictDialogEvent> for ConflictDialog {}

impl ConflictDialog {
    /// A dialog for `source`, previewing its first file.
    pub fn new(source: ConflictSource, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            source,
            selected: None,
            preview: None,
            starting: false,
            cancelling: false,
            error: None,
            preview_task: None,
            action_task: None,
        };
        if !this.source.files().is_empty() {
            this.select(0, cx);
        }
        this
    }

    /// Loads the conflicted content of the file at `index`.
    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(path) = self.source.files().get(index).cloned() else {
            return;
        };
        self.selected = Some(index);
        self.preview = Some(Preview::Loading);
        let runs = cx.global::<AppContext>().runs.clone();
        let source = self.source.clone();
        let job = spawn_job(cx, async move {
            match &source {
                ConflictSource::Accept(conflict) => {
                    runs.preview_accept_conflict(conflict, &path).await
                }
                ConflictSource::Push { request, conflict } => {
                    runs.preview_push_conflict(request, conflict, &path).await
                }
            }
        });
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if this.selected != Some(index) {
                    return;
                }
                this.preview = Some(match result {
                    Ok(Ok(text)) => {
                        let total = text.lines().count();
                        let lines = text
                            .lines()
                            .take(PREVIEW_MAX_LINES)
                            .map(|line| SharedString::from(line.to_owned()))
                            .collect();
                        Preview::Ready(lines, total > PREVIEW_MAX_LINES)
                    }
                    Ok(Err(error)) => Preview::Failed(error.to_string().into()),
                    Err(error) => Preview::Failed(error.to_string().into()),
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Resolve with Claude, with the Run menu's model and effort.
    fn resolve(&mut self, cx: &mut Context<Self>) {
        if self.starting || self.cancelling {
            return;
        }
        let (model, effort) = followup::current(cx);
        let runs = cx.global::<AppContext>().runs.clone();
        let source = self.source.clone();
        // The start must not be cut short by the dialog closing.
        let handle = cx.global::<AppContext>().runtime.spawn(async move {
            match source {
                ConflictSource::Accept(conflict) => {
                    runs.resolve_accept(conflict, model, effort).await
                }
                ConflictSource::Push { request, conflict } => {
                    runs.resolve_push(*request, *conflict, model, effort).await
                }
            }
        });
        self.starting = true;
        self.error = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = handle.await;
            this.update(cx, |this, cx| {
                this.starting = false;
                match result {
                    Ok(Ok(run_id)) => {
                        cx.emit(ConflictDialogEvent::ResolveStarted(run_id));
                        cx.emit(ConflictDialogEvent::Closed);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "cannot start the resolve run");
                        this.error = Some(error.to_string().into());
                    }
                    Err(error) => this.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Cancel (or Escape): nothing is rebased or committed. A
    /// pull-before-push resolution in progress is cancelled: its worktree
    /// is deleted and the Push abandoned.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.starting || self.cancelling {
            return;
        }
        let resolve = match &self.source {
            ConflictSource::Push { conflict, .. } => conflict.resolve.as_ref().map(|r| r.id),
            ConflictSource::Accept(_) => None,
        };
        let Some(id) = resolve else {
            let message = match &self.source {
                ConflictSource::Accept(_) => {
                    "Conflict left as is: nothing was committed and the proposal stays open."
                }
                ConflictSource::Push { .. } => {
                    "Push abandoned: nothing was rebased and nothing was pushed."
                }
            };
            cx.emit(ConflictDialogEvent::Notice {
                message: message.into(),
                is_error: false,
            });
            cx.emit(ConflictDialogEvent::Closed);
            return;
        };
        let runs = cx.global::<AppContext>().runs.clone();
        let handle = cx
            .global::<AppContext>()
            .runtime
            .spawn(async move { runs.cancel_resolve(id).await });
        self.cancelling = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = handle.await;
            this.update(cx, |this, cx| {
                this.cancelling = false;
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(error) => Some(error.to_string()),
                };
                match error {
                    None => {
                        cx.emit(ConflictDialogEvent::Notice {
                            message: "Push abandoned: the rebase in progress was cancelled and \
                                      its worktree deleted. The head branch did not move and \
                                      nothing was pushed."
                                .into(),
                            is_error: false,
                        });
                        cx.emit(ConflictDialogEvent::Closed);
                    }
                    Some(message) => {
                        this.error = Some(format!("Could not cancel: {message}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- rendering ----------------------------------------------------------

    fn description(&self) -> String {
        match &self.source {
            ConflictSource::Accept(conflict) => format!(
                "The proposal does not apply cleanly on the tip of `{}` ({}). Nothing was \
                 committed, the head branch did not move, and the proposal stays open. Resolve \
                 with Claude applies it again in a new worktree, with the conflict markers in \
                 the files, and starts a resolve run; its result comes back as a proposal of \
                 this comment.",
                conflict.head,
                conflict.head_tip.short(8)
            ),
            ConflictSource::Push { conflict, .. } => match &conflict.resolve {
                None => format!(
                    "`{}` is behind `{}` ({}), and its commits do not rebase cleanly on the \
                     remote's. The rebase was aborted: `{}` did not move ({}) and nothing was \
                     pushed. Resolve with Claude replays the rebase in a new worktree, stopped \
                     at the first conflicting commit, and starts a resolve run; accepting its \
                     result continues the rebase, then the push.",
                    conflict.upstream,
                    conflict.upstream,
                    conflict.upstream_tip.short(8),
                    conflict.head,
                    conflict.head_tip.short(8),
                ),
                Some(progress) => format!(
                    "The resolution of the previous commit is in, and the rebase onto `{}` \
                     stopped at the next conflicting local commit, {} “{}”. `{}` has not moved \
                     and nothing was pushed. Resolve with Claude starts the next resolve run in \
                     the same worktree; Cancel deletes it and abandons the push.",
                    progress.upstream,
                    progress.commit.short(8),
                    progress.subject,
                    progress.head,
                ),
            },
        }
    }

    fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .font_family(theme.fonts.code_family.clone())
            .text_sm()
            .children(
                self.source
                    .files()
                    .iter()
                    .enumerate()
                    .map(|(index, file)| {
                        let selected = self.selected == Some(index);
                        div()
                            .id(("conflict-file", index))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(colors.warning)
                            .when(selected, |row| row.bg(colors.element_selected))
                            .hover(|style| style.bg(colors.element_hover))
                            .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx)))
                            .child(format!("U  {}", file.display()))
                    })
                    .collect::<Vec<_>>(),
            )
            .into_any_element()
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let preview = self.preview.as_ref()?;
        let body = match preview {
            Preview::Loading => div()
                .text_color(colors.text_muted)
                .child("Loading the conflicted content…")
                .into_any_element(),
            Preview::Failed(message) => div()
                .text_color(colors.error)
                .child(format!("Cannot preview this file: {message}"))
                .into_any_element(),
            Preview::Ready(lines, truncated) => div()
                .flex()
                .flex_col()
                .children(lines.iter().map(|line| {
                    let marker = ["<<<<<<<", "=======", ">>>>>>>", "|||||||"]
                        .iter()
                        .any(|marker| line.starts_with(marker));
                    div()
                        .whitespace_nowrap()
                        .text_color(if marker { colors.warning } else { colors.text })
                        .when(marker, |row| row.font_weight(FontWeight::SEMIBOLD))
                        .child(if line.is_empty() {
                            SharedString::from(" ")
                        } else {
                            line.clone()
                        })
                }))
                .when(*truncated, |column| {
                    column.child(div().text_color(colors.text_placeholder).child(format!(
                        "… only the first {PREVIEW_MAX_LINES} lines are shown"
                    )))
                })
                .into_any_element(),
        };
        Some(
            div()
                .id("conflict-preview")
                .max_h(px(300.0))
                .overflow_scroll()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.background)
                .font_family(theme.fonts.code_family.clone())
                .text_xs()
                .child(body)
                .into_any_element(),
        )
    }
}

impl Render for ConflictDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let operation = match &self.source {
            ConflictSource::Accept(_) => "OPERATION: ACCEPT".to_owned(),
            ConflictSource::Push { conflict, .. } => {
                format!("OPERATION: {}", conflict.operation.to_uppercase())
            }
        };
        let files = self.render_files(cx);
        let preview = self.render_preview(cx);
        let error = self.error.clone().map(|message| {
            div()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(colors.error.opacity(0.6))
                .bg(colors.error.opacity(0.08))
                .text_sm()
                .text_color(colors.error)
                .child(message)
        });
        let cancel = if self.cancelling {
            disabled_button("conflict-cancel", "Cancelling…", cx).into_any_element()
        } else {
            button("conflict-cancel", "Cancel", cx)
                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)))
                .child(kbd("Esc", cx))
                .into_any_element()
        };
        let resolve = if self.starting {
            disabled_button("conflict-resolve", "Starting…", cx).into_any_element()
        } else if self.cancelling {
            disabled_button("conflict-resolve", "Resolve with Claude", cx).into_any_element()
        } else {
            primary_button("conflict-resolve", "Resolve with Claude", cx)
                .h(px(24.0))
                .on_click(cx.listener(|this, _, _, cx| this.resolve(cx)))
                .into_any_element()
        };
        let picker = div()
            .flex()
            .items_center()
            .gap_1()
            .text_xs()
            .text_color(colors.text_muted)
            .child("Model · effort")
            .child(followup::choice_buttons("conflict", cx));
        div()
            .id("conflict-backdrop")
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
                    .id("conflict-dialog")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w(px(720.0))
                    .max_h(px(680.0))
                    .overflow_y_scroll()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.warning.opacity(0.6))
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
                            .child(div().text_color(colors.warning).child("⚠"))
                            .child(self.source.title()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text_muted)
                            .child(operation),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(colors.text_muted)
                            .child(self.description()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text_muted)
                            .child(format!(
                                "CONFLICTED FILES ({}) · click one to preview it",
                                self.source.files().len()
                            )),
                    )
                    .child(files)
                    .children(preview)
                    .children(error)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(picker)
                            .child(div().flex_1())
                            .child(cancel)
                            .child(resolve),
                    ),
            )
    }
}

/// The first `max` characters of `text`, with an ellipsis when cut.
fn excerpt(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}
