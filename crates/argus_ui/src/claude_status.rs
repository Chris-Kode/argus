//! Mirrors the run manager's [`ClaudeStatus`] into views.
//!
//! Detection itself runs on the tokio runtime (see
//! [`argus_runs::RunManager::start_detection`]); views only receive the
//! result through a `watch` channel and repaint.

use argus_runs::ClaudeStatus;
use gpui::{Context, Task};

use crate::AppContext;

/// Calls `apply` with the current status now and with every later one, until
/// the view is dropped (or the returned task is).
pub fn observe_claude_status<V: 'static>(
    cx: &mut Context<V>,
    apply: impl Fn(&mut V, ClaudeStatus, &mut Context<V>) + 'static,
) -> Task<()> {
    let mut statuses = cx.global::<AppContext>().runs.subscribe_claude();
    cx.spawn(async move |this, cx| {
        loop {
            let status = statuses.borrow_and_update().clone();
            let applied = this.update(cx, |view, cx| {
                apply(view, status, cx);
                cx.notify();
            });
            if applied.is_err() || statuses.changed().await.is_err() {
                return;
            }
        }
    })
}
