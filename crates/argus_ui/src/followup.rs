//! Follow-up runs from the UI (spec §2, §6.2).
//!
//! A reply on a `Proposed` comment can be sent as a follow-up run, which
//! resumes the thread's Claude session in the proposal's worktree, or added
//! as a note only. [`FollowupChoice`] holds the model and effort the next
//! follow-up uses: the run defaults of the settings until the user picks
//! others with [`choice_buttons`], so they can change between follow-ups
//! while the session stays the same. [`spawn_followup`] starts the run on
//! the tokio runtime; the UI thread only waits for the outcome.

use argus_diff::review::CommentId;
use argus_runs::RunError;
use argus_store::RunId;
use gpui::{App, Context, Div, Global, SharedString, Task, prelude::*};

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

/// The model and effort picked for follow-ups in this session of the app.
/// `None` follows the settings' defaults.
#[derive(Clone, Debug, Default)]
pub struct FollowupChoice {
    model: Option<String>,
    effort: Option<String>,
}

impl Global for FollowupChoice {}

/// The model and effort of the next follow-up.
pub fn current(cx: &App) -> (String, String) {
    let settings = cx.global::<AppContext>().runs.settings();
    let choice = cx.try_global::<FollowupChoice>();
    let model = choice
        .and_then(|choice| choice.model.clone())
        .filter(|model| settings.models.contains(model))
        .unwrap_or(settings.default_model);
    let effort = choice
        .and_then(|choice| choice.effort.clone())
        .filter(|effort| settings.efforts.contains(effort))
        .unwrap_or(settings.default_effort);
    (model, effort)
}

/// `model · effort`, as the reply box shows it.
pub fn label(model: &str, effort: &str) -> String {
    if effort.trim().is_empty() {
        model.to_owned()
    } else {
        format!("{model} · {effort}")
    }
}

/// Picks the next model of the settings' list.
pub fn cycle_model(cx: &mut App) {
    let models = cx.global::<AppContext>().runs.settings().models;
    let (model, _) = current(cx);
    let next = next_in(&models, &model);
    cx.default_global::<FollowupChoice>().model = next;
}

/// Picks the next effort of the settings' list.
pub fn cycle_effort(cx: &mut App) {
    let efforts = cx.global::<AppContext>().runs.settings().efforts;
    let (_, effort) = current(cx);
    let next = next_in(&efforts, &effort);
    cx.default_global::<FollowupChoice>().effort = next;
}

fn next_in(list: &[String], current: &str) -> Option<String> {
    let index = list.iter().position(|entry| entry == current);
    let next = index.map_or(0, |index| (index + 1) % list.len().max(1));
    list.get(next).cloned()
}

/// The model and effort buttons of a reply box; a click picks the next
/// entry of the settings' list.
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

/// Starts a follow-up of `comment_id` with the current model and effort.
/// The run is started on the tokio runtime and is not cut short when the
/// view goes away; the task resolves once its process runs (or it failed
/// to start).
pub fn spawn_followup(
    comment_id: CommentId,
    kind: FollowupKind,
    cx: &mut App,
) -> Task<Result<RunId, String>> {
    let (model, effort) = current(cx);
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
