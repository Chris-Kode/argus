//! The comments of the open review as a GPUI entity.
//!
//! [`ReviewComments`] wraps an [`argus_diff::review::CommentService`]: every
//! storage call runs on the tokio runtime through [`spawn_job`], and the
//! service's change broadcast is forwarded to views as
//! [`ReviewCommentsEvent`]s. Views read the in-memory snapshot only. Runs
//! change comment statuses behind the service, so a run that starts or
//! finishes for a shown comment reloads the review.
//!
//! After every diff refresh the workspace calls [`ReviewComments::reanchor`]
//! with the new diff session, so comments follow their code or turn
//! outdated (spec §7.3).

use std::sync::Arc;

use argus_diff::review::{
    Comment, CommentAnchor, CommentChange, CommentEvent, CommentId, CommentService, ReplyAuthor,
    ReviewError, ReviewKey,
};
use argus_diff::{DiffError, DiffSession};
use argus_runs::RunUpdate;
use gpui::{Context, EventEmitter, SharedString, Task};
use tokio::sync::broadcast::error::RecvError;

use crate::AppContext;
use crate::runtime::spawn_job;

/// Events for views showing comments.
#[derive(Clone, Debug)]
pub enum ReviewCommentsEvent {
    /// The comment list changed; re-read [`ReviewComments::comments`].
    /// `None` when changes were missed and the whole list may differ.
    Changed(Option<CommentChange>),
    /// A comment operation failed.
    Error(SharedString),
}

/// The comment threads of the current (repo, base, head) review.
pub struct ReviewComments {
    service: CommentService,
    comments: Arc<Vec<Comment>>,
    /// The review last asked for; the service catches up once its load runs.
    requested: Option<ReviewKey>,
    load_task: Option<Task<()>>,
    /// The re-anchoring pass in flight; replacing it cancels it.
    reanchor_task: Option<Task<()>>,
    _listen_task: Task<()>,
    _runs_task: Task<()>,
}

impl EventEmitter<ReviewCommentsEvent> for ReviewComments {}

impl ReviewComments {
    /// An empty list, bound to the store of the [`AppContext`] global.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let service = CommentService::new(cx.global::<AppContext>().store.comments());
        let mut changes = service.subscribe();
        let listen_task = cx.spawn(async move |this, cx| {
            loop {
                let change = match changes.recv().await {
                    Ok(change) => Some(change),
                    // Missed changes are covered by re-reading the snapshot.
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => break,
                };
                let alive = this
                    .update(cx, |this, cx| {
                        this.comments = this.service.comments();
                        cx.emit(ReviewCommentsEvent::Changed(change));
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        let mut runs = cx.global::<AppContext>().runs.subscribe_runs();
        let runs_task = cx.spawn(async move |this, cx| {
            loop {
                let comment_id = match runs.recv().await {
                    Ok(RunUpdate::Started { comment_id, .. }) => comment_id,
                    Ok(RunUpdate::Finished(finished)) => finished.comment_id,
                    Err(RecvError::Lagged(_)) => None,
                    Err(RecvError::Closed) => break,
                };
                let alive = this
                    .update(cx, |this, cx| {
                        let shown =
                            comment_id.is_none_or(|id| this.comments.iter().any(|c| c.id == id));
                        if shown {
                            this.reload(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        Self {
            comments: service.comments(),
            requested: None,
            service,
            load_task: None,
            reanchor_task: None,
            _listen_task: listen_task,
            _runs_task: runs_task,
        }
    }

    /// The underlying service, for components that drive statuses (runs).
    pub fn service(&self) -> &CommentService {
        &self.service
    }

    /// The review being shown (or loading).
    pub fn key(&self) -> Option<&ReviewKey> {
        self.requested.as_ref()
    }

    /// Whether the comments of the review are still loading.
    pub fn is_loading(&self) -> bool {
        self.load_task.is_some()
    }

    /// The loaded comments, by file, line and creation time.
    pub fn comments(&self) -> &Arc<Vec<Comment>> {
        &self.comments
    }

    /// Shows the comments of `key` (`None` clears them). A no-op when that
    /// review is already loaded; replacing the task cancels a load in flight.
    pub fn set_review(&mut self, key: Option<ReviewKey>, cx: &mut Context<Self>) {
        if self.requested == key {
            return;
        }
        self.requested = key.clone();
        self.load(key, cx);
    }

    /// Reloads the shown review from storage, e.g. after a run or an Accept
    /// changed comments behind the service.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let key = self.requested.clone();
        if key.is_some() {
            self.load(key, cx);
        }
    }

    fn load(&mut self, key: Option<ReviewKey>, cx: &mut Context<Self>) {
        let service = self.service.clone();
        let job = spawn_job(cx, async move { service.load(key).await });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.load_task = None;
                report(flatten(result), "load comments", cx);
            })
            .ok();
        }));
    }

    /// Re-anchors the shown review's comments over `session`, the refreshed
    /// diff of the same (base, head), off the UI thread. Changes are
    /// persisted in one transaction and arrive as a
    /// [`CommentChange::Reanchored`]. A later call cancels a pass still in
    /// flight; a pass run before the comments are loaded does nothing, so
    /// call it again on [`CommentChange::Loaded`].
    pub fn reanchor(&mut self, session: DiffSession, cx: &mut Context<Self>) {
        let Some(key) = self.requested.clone() else {
            return;
        };
        let service = self.service.clone();
        let job = spawn_job(cx, async move {
            match service.reanchor(&key, &session).await {
                Ok(changes) => {
                    if !changes.is_empty() {
                        tracing::debug!(changed = changes.len(), "re-anchored comments");
                    }
                    Ok(())
                }
                // The diff session went away: a newer pass follows.
                Err(ReviewError::Diff(DiffError::Cancelled)) => Ok(()),
                Err(error) => Err(error),
            }
        });
        self.reanchor_task = Some(cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            this.update(cx, |this, cx| {
                this.reanchor_task = None;
                report(result, "re-anchor the comments", cx);
            })
            .ok();
        }));
    }

    /// Re-attaches a comment to a range picked in the diff: its anchor,
    /// `anchor_sha`, context hash and snippet are replaced and it is no
    /// longer outdated.
    pub fn reattach(
        &mut self,
        id: CommentId,
        anchor: CommentAnchor,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let service = self.service.clone();
        self.run(cx, "re-attach the comment", async move {
            service.reattach(id, anchor).await
        })
    }

    /// Creates a comment on the loaded review. The task resolves to whether
    /// it was saved (a failure is also reported as an error event); detach
    /// it when the outcome does not matter.
    pub fn create(
        &mut self,
        anchor: CommentAnchor,
        body_md: String,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let service = self.service.clone();
        self.run(cx, "create the comment", async move {
            service.create(anchor, body_md).await.map(drop)
        })
    }

    /// Replaces a comment's body.
    pub fn edit(&mut self, id: CommentId, body_md: String, cx: &mut Context<Self>) -> Task<bool> {
        let service = self.service.clone();
        self.run(cx, "edit the comment", async move {
            service.edit(id, body_md).await
        })
    }

    /// Adds a reviewer reply to a thread.
    pub fn reply(&mut self, id: CommentId, body_md: String, cx: &mut Context<Self>) -> Task<bool> {
        let service = self.service.clone();
        self.run(cx, "reply", async move {
            service
                .reply(id, ReplyAuthor::User, body_md)
                .await
                .map(drop)
        })
    }

    /// Deletes a thread with its replies.
    pub fn delete(&mut self, id: CommentId, cx: &mut Context<Self>) -> Task<bool> {
        let service = self.service.clone();
        self.run(
            cx,
            "delete the comment",
            async move { service.delete(id).await },
        )
    }

    /// Applies a status event (e.g. Dismiss, Reopen).
    pub fn apply(
        &mut self,
        id: CommentId,
        event: CommentEvent,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let service = self.service.clone();
        self.run(cx, "update the comment", async move {
            service.apply(id, event).await.map(drop)
        })
    }

    /// Runs a write on the tokio runtime to completion (a write must not be
    /// cut short by the view going away) and reports its failure. The task
    /// resolves to whether the write succeeded; dropping it only skips the
    /// report.
    fn run(
        &mut self,
        cx: &mut Context<Self>,
        what: &'static str,
        future: impl Future<Output = Result<(), ReviewError>> + Send + 'static,
    ) -> Task<bool> {
        let handle = cx.global::<AppContext>().runtime.spawn(future);
        cx.spawn(async move |this, cx| {
            let result = flatten(handle.await);
            let ok = result.is_ok();
            this.update(cx, |_, cx| report(result, what, cx)).ok();
            ok
        })
    }
}

fn flatten(result: Result<Result<(), ReviewError>, tokio::task::JoinError>) -> Result<(), String> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn report(result: Result<(), String>, what: &str, cx: &mut Context<ReviewComments>) {
    if let Err(message) = result {
        tracing::warn!(%message, "could not {what}");
        cx.emit(ReviewCommentsEvent::Error(
            format!("Could not {what}: {message}").into(),
        ));
    }
}
