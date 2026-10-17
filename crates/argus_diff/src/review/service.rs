//! [`CommentService`]: the comments of the open review, kept in memory and
//! persisted through [`CommentStore`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use argus_store::{CommentId, CommentStatus, CommentStore, NewComment, Reply, ReplyAuthor};
use tokio::sync::broadcast;

use crate::review::anchor::CommentAnchor;
use crate::review::comment::{Comment, ReviewKey, store_anchor};
use crate::review::error::{ReviewError, ReviewResult};
use crate::review::reanchor::{self, AnchorChange};
use crate::review::status::{CommentEvent, transition};
use crate::session::DiffSession;

/// Change notifications, buffered per subscriber. A subscriber that lags
/// behind gets `RecvError::Lagged` and should re-read the snapshot.
const CHANGE_CAPACITY: usize = 256;

/// How often [`apply_event`] re-reads a status that changed under it before
/// giving up.
const TRANSITION_ATTEMPTS: usize = 4;

/// What changed in the [`CommentService`]. Subscribers re-read
/// [`CommentService::comments`] (or [`CommentService::get`]) on each one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommentChange {
    /// Another review was selected; its comments are being loaded and the
    /// list is empty until [`CommentChange::Loaded`].
    Loading(ReviewKey),
    /// The comments of the review were (re)loaded from storage.
    Loaded(ReviewKey),
    /// The loaded review was cleared; no comments are shown.
    Cleared,
    /// The review failed to load; the list stays empty.
    LoadFailed(ReviewKey),
    /// A comment of the loaded review was created.
    Created(CommentId),
    /// A comment (its body, status, anchor or replies) changed.
    Updated(CommentId),
    /// A comment and its replies were deleted.
    Deleted(CommentId),
    /// Re-anchoring moved these comments, or changed their outdated flag.
    Reanchored(Vec<CommentId>),
}

#[derive(Default)]
struct State {
    key: Option<ReviewKey>,
    /// Bumped on every review switch, so a slow load of a previous review
    /// never overwrites the current one.
    generation: u64,
    /// Sorted by file, line and creation time, like the store lists them.
    comments: Arc<Vec<Comment>>,
}

/// The comment threads of the current (repo, base, head) review.
///
/// Cheap to clone; clones share the same state. Every method that touches
/// storage is async and must run on the tokio runtime, never on the UI
/// thread; reading the in-memory snapshot is synchronous and cheap. Each
/// change is broadcast to [`CommentService::subscribe`]rs.
#[derive(Clone)]
pub struct CommentService {
    store: CommentStore,
    state: Arc<Mutex<State>>,
    changes: broadcast::Sender<CommentChange>,
}

impl std::fmt::Debug for CommentService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("CommentService")
            .field("key", &state.key)
            .field("comments", &state.comments.len())
            .finish()
    }
}

impl CommentService {
    /// A service with no review loaded.
    pub fn new(store: CommentStore) -> Self {
        let (changes, _) = broadcast::channel(CHANGE_CAPACITY);
        Self {
            store,
            state: Arc::new(Mutex::new(State::default())),
            changes,
        }
    }

    /// Subscribes to change notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<CommentChange> {
        self.changes.subscribe()
    }

    /// The loaded review, if any.
    pub fn key(&self) -> Option<ReviewKey> {
        self.lock().key.clone()
    }

    /// Snapshot of the loaded comments, by file, line and creation time.
    pub fn comments(&self) -> Arc<Vec<Comment>> {
        self.lock().comments.clone()
    }

    /// One loaded comment.
    pub fn get(&self, id: CommentId) -> Option<Comment> {
        self.lock().comments.iter().find(|c| c.id == id).cloned()
    }

    /// Selects the review to show and loads its comments with their replies.
    /// `None` clears the list. Selecting the review that is already loaded
    /// reloads it in place.
    pub async fn load(&self, key: Option<ReviewKey>) -> ReviewResult<()> {
        let generation = {
            let mut state = self.lock();
            state.generation += 1;
            let switched = state.key != key;
            if switched {
                state.key = key.clone();
                state.comments = Arc::new(Vec::new());
                self.emit(match &key {
                    Some(key) => CommentChange::Loading(key.clone()),
                    None => CommentChange::Cleared,
                });
            }
            state.generation
        };
        let Some(key) = key else {
            return Ok(());
        };

        let loaded = self.read_review(&key).await;
        let mut state = self.lock();
        if state.generation != generation {
            return Ok(());
        }
        match loaded {
            Ok(comments) => {
                tracing::debug!(
                    repo = %key.repo_id,
                    base = %key.base_ref,
                    head = %key.head_ref,
                    count = comments.len(),
                    "loaded comments"
                );
                state.comments = Arc::new(comments);
                self.emit(CommentChange::Loaded(key));
                Ok(())
            }
            Err(error) => {
                self.emit(CommentChange::LoadFailed(key));
                Err(error)
            }
        }
    }

    /// Reloads the current review from storage, e.g. after another component
    /// wrote to it.
    pub async fn reload(&self) -> ReviewResult<()> {
        let key = self.key();
        match key {
            Some(key) => self.load(Some(key)).await,
            None => Ok(()),
        }
    }

    /// Creates an `Open` comment on the loaded review.
    pub async fn create(&self, anchor: CommentAnchor, body_md: String) -> ReviewResult<Comment> {
        let key = self.key().ok_or(ReviewError::NoReview)?;
        let stored_anchor = store_anchor(&anchor);
        let stored = self
            .store
            .create(NewComment {
                repo_id: key.repo_id,
                base_ref: key.base_ref,
                head_ref: key.head_ref,
                anchor: stored_anchor,
                anchor_sha: anchor.anchor_sha,
                snippet: anchor.snippet,
                body_md,
            })
            .await?;
        let comment = Comment::from_stored(stored, Vec::new());
        tracing::debug!(comment = %comment.id, "created comment");
        self.install(comment.id, Some(comment.clone()));
        Ok(comment)
    }

    /// Replaces the body of a root comment.
    pub async fn edit(&self, id: CommentId, body_md: String) -> ReviewResult<()> {
        self.store.update_body(id, body_md).await?;
        self.refresh(id).await
    }

    /// Appends a reply to a thread.
    pub async fn reply(
        &self,
        id: CommentId,
        author: ReplyAuthor,
        body_md: String,
    ) -> ReviewResult<Reply> {
        let reply = self.store.add_reply(id, author, body_md).await?;
        self.refresh(id).await?;
        Ok(reply)
    }

    /// Deletes a thread. Its replies go with it; its runs stay for the audit
    /// trail, with their comment link cleared.
    pub async fn delete(&self, id: CommentId) -> ReviewResult<()> {
        self.store.delete(id).await?;
        tracing::debug!(comment = %id, "deleted comment");
        self.install(id, None);
        Ok(())
    }

    /// Moves a comment through the state machine (spec §7.2) and persists
    /// the new status. An invalid event is refused and changes nothing.
    pub async fn apply(&self, id: CommentId, event: CommentEvent) -> ReviewResult<CommentStatus> {
        let status = apply_event(&self.store, id, event).await?;
        self.refresh(id).await?;
        Ok(status)
    }

    /// Moves a comment to a new location (re-anchoring, spec §7.3) and clears
    /// its outdated flag. The original `anchor_sha` and `snippet` are kept.
    pub async fn set_anchor(&self, id: CommentId, anchor: &CommentAnchor) -> ReviewResult<()> {
        self.store.set_anchor(id, store_anchor(anchor)).await?;
        self.refresh(id).await
    }

    /// Re-attaches a comment (typically an outdated one) to a range the user
    /// picked: the anchor, `anchor_sha`, context hash and snippet all come
    /// from `anchor`, and the outdated flag is cleared.
    pub async fn reattach(&self, id: CommentId, anchor: CommentAnchor) -> ReviewResult<()> {
        let stored = store_anchor(&anchor);
        self.store
            .reattach(id, stored, anchor.anchor_sha, anchor.snippet)
            .await?;
        tracing::debug!(comment = %id, "re-attached comment");
        self.refresh(id).await
    }

    /// Re-anchors the comments of `key` over a refreshed diff (spec §7.3)
    /// and persists the changes in one transaction. Does nothing unless
    /// `key` is the loaded review. Returns the changes that were applied; a
    /// comment edited meanwhile keeps its edit.
    pub async fn reanchor(
        &self,
        key: &ReviewKey,
        session: &DiffSession,
    ) -> ReviewResult<Vec<AnchorChange>> {
        let (generation, snapshot) = {
            let state = self.lock();
            if state.key.as_ref() != Some(key) {
                return Ok(Vec::new());
            }
            (state.generation, state.comments.clone())
        };
        let changes = reanchor::reanchor(snapshot, session).await?;
        if changes.is_empty() {
            return Ok(changes);
        }
        let updates = changes.iter().map(AnchorChange::to_update).collect();
        let applied: HashSet<CommentId> = self
            .store
            .apply_anchor_updates(updates)
            .await?
            .into_iter()
            .collect();
        let applied: Vec<AnchorChange> = changes
            .into_iter()
            .filter(|change| applied.contains(&change.id))
            .collect();
        if applied.is_empty() {
            return Ok(applied);
        }
        let reload = {
            let mut state = self.lock();
            if state.generation == generation {
                let comments = Arc::make_mut(&mut state.comments);
                for change in &applied {
                    if let Some(comment) = comments.iter_mut().find(|c| c.id == change.id) {
                        change.apply_to(comment);
                    }
                }
                comments.sort_by(|a, b| order(a).cmp(&order(b)));
                self.emit(CommentChange::Reanchored(
                    applied.iter().map(|change| change.id).collect(),
                ));
                false
            } else {
                // A load ran meanwhile and may have read the rows before
                // this write.
                true
            }
        };
        tracing::debug!(changed = applied.len(), "persisted re-anchored comments");
        if reload {
            self.reload().await?;
        }
        Ok(applied)
    }

    /// Flags a comment as outdated: its code is gone from the diff.
    pub async fn mark_outdated(&self, id: CommentId) -> ReviewResult<()> {
        self.store.mark_outdated(id).await?;
        self.refresh(id).await
    }

    /// Reads all comments of `key` with their replies.
    async fn read_review(&self, key: &ReviewKey) -> ReviewResult<Vec<Comment>> {
        let stored = self
            .store
            .list_for(key.repo_id, key.base_ref.clone(), key.head_ref.clone())
            .await?;
        let replies = self
            .store
            .replies_for(key.repo_id, key.base_ref.clone(), key.head_ref.clone())
            .await?;
        let mut by_comment: HashMap<CommentId, Vec<Reply>> = HashMap::new();
        for reply in replies {
            by_comment.entry(reply.comment_id).or_default().push(reply);
        }
        Ok(stored
            .into_iter()
            .map(|comment| {
                let replies = by_comment.remove(&comment.id).unwrap_or_default();
                Comment::from_stored(comment, replies)
            })
            .collect())
    }

    /// Re-reads one comment from storage into the snapshot.
    async fn refresh(&self, id: CommentId) -> ReviewResult<()> {
        let comment = match self.store.get(id).await? {
            Some(stored) => {
                let replies = self.store.replies(id).await?;
                Some(Comment::from_stored(stored, replies))
            }
            None => None,
        };
        self.install(id, comment);
        Ok(())
    }

    /// Puts `comment` (or its absence) into the snapshot when it belongs to
    /// the loaded review, and notifies subscribers.
    fn install(&self, id: CommentId, comment: Option<Comment>) {
        let mut state = self.lock();
        let key = state.key.clone();
        let comment = comment.filter(|comment| key.as_ref().is_some_and(|key| key.is_of(comment)));
        let position = state.comments.iter().position(|c| c.id == id);
        let change = match (comment, position) {
            (None, None) => return,
            (None, Some(index)) => {
                Arc::make_mut(&mut state.comments).remove(index);
                CommentChange::Deleted(id)
            }
            (Some(comment), position) => {
                let comments = Arc::make_mut(&mut state.comments);
                let change = match position {
                    Some(index) => {
                        comments.remove(index);
                        CommentChange::Updated(id)
                    }
                    None => CommentChange::Created(id),
                };
                let at = comments.partition_point(|c| order(c) <= order(&comment));
                comments.insert(at, comment);
                change
            }
        };
        self.emit(change);
    }

    fn emit(&self, change: CommentChange) {
        // No subscriber is fine: nothing is showing the comments yet.
        let _ = self.changes.send(change);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The list order: by file, line and creation time.
fn order(comment: &Comment) -> (&str, u32, i64) {
    (
        comment.anchor.file_path.as_str(),
        comment.anchor.start_line,
        comment.created_at,
    )
}

/// Applies `event` to a stored comment: reads its current status, runs
/// [`transition`] and writes the result with a compare-and-swap, so a status
/// that changed concurrently is re-read instead of overwritten. Returns the
/// new status.
///
/// Outdated comments cannot start a run (spec Q5). Usable without a
/// [`CommentService`], e.g. by startup recovery and the run manager.
pub async fn apply_event(
    store: &CommentStore,
    id: CommentId,
    event: CommentEvent,
) -> ReviewResult<CommentStatus> {
    for _ in 0..TRANSITION_ATTEMPTS {
        let comment = store.get(id).await?.ok_or_else(|| {
            ReviewError::Store(argus_store::StoreError::NotFound {
                entity: "comment",
                id: id.to_string(),
            })
        })?;
        if event == CommentEvent::RunStarted && comment.outdated {
            return Err(ReviewError::Outdated(id));
        }
        let next = transition(comment.status, event)?;
        if store.set_status_if(id, comment.status, next).await? {
            tracing::debug!(comment = %id, from = %comment.status, to = %next, %event, "comment status");
            return Ok(next);
        }
    }
    Err(ReviewError::Contended(id))
}
