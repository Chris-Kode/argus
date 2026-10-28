//! Follow-up runs from the UI (spec §2, §6.2).
//!
//! A reply on a `Proposed` comment can be sent as a follow-up run, which
//! resumes the thread's Claude session in the proposal's worktree, or added
//! as a note only. A follow-up uses the Run menu's model and effort: the
//! thread's Run menu pick, or the Run menu defaults (the last choice),
//! which the proposal tab's [`choice_buttons`] change. They can change
//! between follow-ups while the session stays the same. [`spawn_followup`]
//! starts the run on the tokio runtime; the UI thread only waits for the
//! outcome.

use argus_diff::review::CommentId;
use argus_runs::RunError;
use argus_runs::settings::{EFFORTS, MODELS};
use argus_store::RunId;
use gpui::{App, Context, Div, SharedString, Task, prelude::*};

use crate::AppContext;
use crate::components::button;
use crate::theme::ActiveTheme;

/// What a follow-up sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowupKind {
    /// Resume the session with this reply (saved in the thread).
    Resume(String),
    /// A new session with the whole thread as context, for a thread whose
    /// session could not be resumed.
    Fresh,
}

/// The Run menu defaults: the model and effort of the last run.
pub fn current(cx: &App) -> (String, String) {
    let settings = cx.global::<AppContext>().runs.settings();
    (settings.default_model, settings.default_effort)
}

/// `model · effort`, as the reply box shows it.
pub fn label(model: &str, effort: &str) -> String {
    if effort.trim().is_empty() {
        model.to_owned()
    } else {
        format!("{model} · {effort}")
    }
}

/// Makes the next offered model the Run menu default.
pub fn cycle_model(cx: &mut App) {
    let settings = cx.global::<AppContext>().runs.settings();
    if let Some(model) = next_in(&MODELS, &settings.default_model) {
        remember(model, settings.default_effort, cx);
    }
}

/// Makes the next offered effort the Run menu default.
pub fn cycle_effort(cx: &mut App) {
    let settings = cx.global::<AppContext>().runs.settings();
    if let Some(effort) = next_in(&EFFORTS, &settings.default_effort) {
        remember(settings.default_model, effort, cx);
    }
}

/// Stores a new Run menu default; the settings are saved off the UI thread.
fn remember(model: String, effort: String, cx: &mut App) {
    let app = cx.global::<AppContext>();
    let runs = app.runs.clone();
    if runs.remember_choice(&model, &effort) {
        app.runtime.spawn(async move {
            if let Err(error) = runs.save_settings().await {
                tracing::warn!(%error, "cannot save the Run menu defaults");
            }
        });
    }
}

/// The entry after `current` in `list` (the first when `current` is not
/// in it), wrapping around.
pub(crate) fn next_in(list: &[&str], current: &str) -> Option<String> {
    let index = list.iter().position(|&entry| entry == current);
    let next = index.map_or(0, |index| (index + 1) % list.len().max(1));
    list.get(next).map(|&entry| entry.to_owned())
}

/// The model and effort buttons of a reply box (the Run menu defaults); a
/// click picks the next offered entry.
pub fn choice_buttons<V: 'static>(id: &str, cx: &mut Context<V>) -> Div {
    let (model, effort) = current(cx);
    let colors = cx.theme().colors.clone();
    let model_button = button(SharedString::from(format!("{id}-model")), model, cx)
        .h(gpui::px(20.0))
        .text_xs()
        .text_color(colors.text_muted)
        .on_click(cx.listener(|_, _, _, cx| {
            cycle_model(cx);
            cx.notify();
        }));
    let effort_button = (!effort.trim().is_empty()).then(|| {
        button(SharedString::from(format!("{id}-effort")), effort, cx)
            .h(gpui::px(20.0))
            .text_xs()
            .text_color(colors.text_muted)
            .on_click(cx.listener(|_, _, _, cx| {
                cycle_effort(cx);
                cx.notify();
            }))
    });
    gpui::div()
        .flex()
        .flex_none()
        .items_center()
        .gap_0p5()
        .child(model_button)
        .children(effort_button)
}

/// Starts a follow-up of `comment_id` with `model` and `effort`. The run
/// is started on the tokio runtime and is not cut short when the view goes
/// away; the task resolves once its process runs (or it failed to start).
pub fn spawn_followup(
    comment_id: CommentId,
    kind: FollowupKind,
    (model, effort): (String, String),
    cx: &mut App,
) -> Task<Result<RunId, String>> {
    let runs = cx.global::<AppContext>().runs.clone();
    let handle = cx.global::<AppContext>().runtime.spawn(async move {
        match kind {
            FollowupKind::Resume(reply) => runs.follow_up(comment_id, reply, model, effort).await,
            FollowupKind::Fresh => runs.follow_up_fresh(comment_id, model, effort).await,
        }
    });
    cx.background_executor().spawn(async move {
        match handle.await {
            Ok(Ok(run_id)) => Ok(run_id),
            Ok(Err(error)) => Err(describe(&error)),
            Err(error) => Err(error.to_string()),
        }
    })
}

/// Starts a follow-up on the proposal of `run_id` (a pull-before-push
/// resolution, which has no comment) with `reply`, `model` and `effort`;
/// like [`spawn_followup`], it is not cut short when the view goes away.
pub fn spawn_run_followup(
    run_id: RunId,
    reply: String,
    (model, effort): (String, String),
    cx: &mut App,
) -> Task<Result<RunId, String>> {
    let runs = cx.global::<AppContext>().runs.clone();
    let handle = cx
        .global::<AppContext>()
        .runtime
        .spawn(async move { runs.follow_up_run(run_id, reply, model, effort).await });
    cx.background_executor().spawn(async move {
        match handle.await {
            Ok(Ok(run_id)) => Ok(run_id),
            Ok(Err(error)) => Err(describe(&error)),
            Err(error) => Err(error.to_string()),
        }
    })
}

/// A start error as the status bar shows it.
fn describe(error: &RunError) -> String {
    match error {
        RunError::FollowupUnavailable(_) => "The proposal's worktree is gone, so there is \
                                             nothing to follow up on. Start a new run on the \
                                             comment instead."
            .to_owned(),
        other => other.to_string(),
    }
}
