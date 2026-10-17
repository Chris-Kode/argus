//! The `comments` and `comment_replies` tables.

use rusqlite::{OptionalExtension as _, Row, params};

use crate::error::Result;
use crate::model::{CommentId, CommentStatus, ReplyAuthor, ReplyId, RepoId, RunId, Side};
use crate::{StorageService, expect_changed, now_ms};

/// Fields needed to create a comment thread.
#[derive(Clone, Debug, PartialEq)]
pub struct NewComment {
    /// Repository the comment belongs to.
    pub repo_id: RepoId,
    /// Base branch of the reviewed diff.
    pub base_ref: String,
    /// Head branch of the reviewed diff.
    pub head_ref: String,
    /// Where the comment points in the diff.
    pub anchor: Anchor,
    /// Head SHA when the comment was created.
    pub anchor_sha: String,
    /// Original code under the comment, shown once it becomes outdated.
    pub snippet: String,
    /// Markdown body.
    pub body_md: String,
}

/// The location a comment is anchored to (spec §7.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// Path on the new side.
    pub file_path: String,
    /// Path on the old side, for renames.
    pub old_file_path: Option<String>,
    /// Side of the diff the line numbers refer to.
    pub side: Side,
    /// First commented line, 1-based.
    pub start_line: u32,
    /// Last commented line, 1-based, inclusive.
    pub end_line: u32,
    /// Hash of the commented lines plus surrounding context.
    pub context_hash: String,
}

/// One re-anchoring result to persist (spec §7.3): the comment moves from
/// `expected` to `anchor`, or only changes its outdated flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorUpdate {
    /// The comment.
    pub id: CommentId,
    /// The anchor the update was computed from; the row is left alone when
    /// it no longer has it (e.g. the user re-attached it meanwhile).
    pub expected: Anchor,
    /// The outdated flag the update was computed from.
    pub expected_outdated: bool,
    /// The new anchor.
    pub anchor: Anchor,
    /// The code under the new anchor, with its context.
    pub snippet: String,
    /// Whether the code is gone from the diff.
    pub outdated: bool,
}

/// A stored comment thread root.
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    /// Identifier.
    pub id: CommentId,
    /// Repository the comment belongs to.
    pub repo_id: RepoId,
    /// Base branch of the reviewed diff.
    pub base_ref: String,
    /// Head branch of the reviewed diff.
    pub head_ref: String,
    /// Current anchor.
    pub anchor: Anchor,
    /// Head SHA when the comment was created.
    pub anchor_sha: String,
    /// Original code under the comment.
    pub snippet: String,
    /// Markdown body.
    pub body_md: String,
    /// Workflow status.
    pub status: CommentStatus,
    /// Set when re-anchoring found no match.
    pub outdated: bool,
    /// Creation time, unix ms.
    pub created_at: i64,
    /// Last modification time, unix ms.
    pub updated_at: i64,
}

/// A reply in a comment thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// Identifier.
    pub id: ReplyId,
    /// Thread the reply belongs to.
    pub comment_id: CommentId,
    /// Who wrote it.
    pub author: ReplyAuthor,
    /// Markdown body.
    pub body_md: String,
    /// Creation time, unix ms.
    pub created_at: i64,
    /// The run that wrote it (Claude's replies), while the run exists.
    pub run: Option<ReplyRun>,
}

/// The run behind a Claude reply, as the thread shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyRun {
    /// The run.
    pub id: RunId,
    /// Model it used.
    pub model: String,
    /// Thinking effort it used (empty for Claude's default).
    pub effort: String,
}

/// Columns of [`reply_from_row`], for `comment_replies r LEFT JOIN runs u`.
const REPLY_COLUMNS: &str =
    "r.id, r.comment_id, r.author, r.body_md, r.created_at, u.id, u.model, u.effort";

const COLUMNS: &str = "id, repo_id, base_ref, head_ref, file_path, old_file_path, side, \
     start_line, end_line, context_hash, anchor_sha, snippet, body_md, status, outdated, \
     created_at, updated_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<Comment> {
    Ok(Comment {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        base_ref: row.get(2)?,
        head_ref: row.get(3)?,
        anchor: Anchor {
            file_path: row.get(4)?,
            old_file_path: row.get(5)?,
            side: row.get(6)?,
            start_line: row.get(7)?,
            end_line: row.get(8)?,
            context_hash: row.get(9)?,
        },
        anchor_sha: row.get(10)?,
        snippet: row.get(11)?,
        body_md: row.get(12)?,
        status: row.get(13)?,
        outdated: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn reply_from_row(row: &Row<'_>) -> rusqlite::Result<Reply> {
    Ok(Reply {
        id: row.get(0)?,
        comment_id: row.get(1)?,
        author: row.get(2)?,
        body_md: row.get(3)?,
        created_at: row.get(4)?,
        run: match row.get::<_, Option<RunId>>(5)? {
            Some(id) => Some(ReplyRun {
                id,
                model: row.get(6)?,
                effort: row.get(7)?,
            }),
            None => None,
        },
    })
}

/// Async access to `comments` and `comment_replies`.
#[derive(Clone, Debug)]
pub struct CommentStore {
    service: StorageService,
}

impl CommentStore {
    pub(crate) fn new(service: StorageService) -> Self {
        Self { service }
    }

    /// Creates an `Open`, non-outdated comment thread.
    pub async fn create(&self, new: NewComment) -> Result<Comment> {
        self.service
            .call("CommentStore::create", move |conn| {
                let now = now_ms();
                let comment = Comment {
                    id: CommentId::new(),
                    repo_id: new.repo_id,
                    base_ref: new.base_ref,
                    head_ref: new.head_ref,
                    anchor: new.anchor,
                    anchor_sha: new.anchor_sha,
                    snippet: new.snippet,
                    body_md: new.body_md,
                    status: CommentStatus::Open,
                    outdated: false,
                    created_at: now,
                    updated_at: now,
                };
                conn.execute(
                    &format!(
                        "INSERT INTO comments ({COLUMNS}) VALUES \
                         (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
                    ),
                    params![
                        comment.id,
                        comment.repo_id,
                        comment.base_ref,
                        comment.head_ref,
                        comment.anchor.file_path,
                        comment.anchor.old_file_path,
                        comment.anchor.side,
                        comment.anchor.start_line,
                        comment.anchor.end_line,
                        comment.anchor.context_hash,
                        comment.anchor_sha,
                        comment.snippet,
                        comment.body_md,
                        comment.status,
                        comment.outdated,
                        comment.created_at,
                        comment.updated_at,
                    ],
                )?;
                Ok(comment)
            })
            .await
    }

    /// Loads one comment.
    pub async fn get(&self, id: CommentId) -> Result<Option<Comment>> {
        self.service
            .call("CommentStore::get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {COLUMNS} FROM comments WHERE id = ?1"),
                        [id],
                        from_row,
                    )
                    .optional()?)
            })
            .await
    }

    /// Lists the comments of one (repo, base, head) review, by file and line.
    pub async fn list_for(
        &self,
        repo_id: RepoId,
        base_ref: String,
        head_ref: String,
    ) -> Result<Vec<Comment>> {
        self.service
            .call("CommentStore::list_for", move |conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM comments
                     WHERE repo_id = ?1 AND base_ref = ?2 AND head_ref = ?3
                     ORDER BY file_path, start_line, created_at"
                ))?;
                let rows = stmt.query_map(params![repo_id, base_ref, head_ref], from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }

    /// Replaces the markdown body.
    pub async fn update_body(&self, id: CommentId, body_md: String) -> Result<()> {
        self.service
            .call("CommentStore::update_body", move |conn| {
                let changed = conn.execute(
                    "UPDATE comments SET body_md = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, body_md, now_ms()],
                )?;
                expect_changed(changed, "comment", id)
            })
            .await
    }

    /// Sets the workflow status unconditionally. Transition rules (spec §7.2)
    /// are enforced by the caller (`argus_diff::review::apply_event`, which
    /// uses [`CommentStore::set_status_if`]).
    pub async fn set_status(&self, id: CommentId, status: CommentStatus) -> Result<()> {
        self.service
            .call("CommentStore::set_status", move |conn| {
                let changed = conn.execute(
                    "UPDATE comments SET status = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, status, now_ms()],
                )?;
                expect_changed(changed, "comment", id)
            })
            .await
    }

    /// Sets the workflow status only if it is still `expected`, so a status
    /// computed from a stale read is never applied. Returns `false` when the
    /// status had changed in the meantime.
    pub async fn set_status_if(
        &self,
        id: CommentId,
        expected: CommentStatus,
        status: CommentStatus,
    ) -> Result<bool> {
        self.service
            .call("CommentStore::set_status_if", move |conn| {
                let changed = conn.execute(
                    "UPDATE comments SET status = ?3, updated_at = ?4
                     WHERE id = ?1 AND status = ?2",
                    params![id, expected, status, now_ms()],
                )?;
                if changed > 0 {
                    return Ok(true);
                }
                let exists = conn
                    .query_row("SELECT 1 FROM comments WHERE id = ?1", [id], |_| Ok(()))
                    .optional()?
                    .is_some();
                if exists {
                    Ok(false)
                } else {
                    expect_changed(0, "comment", id).map(|()| false)
                }
            })
            .await
    }

    /// Moves the comment to a new anchor and clears its outdated flag.
    pub async fn set_anchor(&self, id: CommentId, anchor: Anchor) -> Result<()> {
        self.service
            .call("CommentStore::set_anchor", move |conn| {
                let changed = conn.execute(
                    "UPDATE comments SET file_path = ?2, old_file_path = ?3, side = ?4,
                         start_line = ?5, end_line = ?6, context_hash = ?7, outdated = 0,
                         updated_at = ?8
                     WHERE id = ?1",
                    params![
                        id,
                        anchor.file_path,
                        anchor.old_file_path,
                        anchor.side,
                        anchor.start_line,
                        anchor.end_line,
                        anchor.context_hash,
                        now_ms(),
                    ],
                )?;
                expect_changed(changed, "comment", id)
            })
            .await
    }

    /// Persists the results of one re-anchoring pass in a single
    /// transaction. Each row is updated only if it still has the anchor and
    /// outdated flag the update was computed from, so a concurrent edit
    /// wins; deleted comments are skipped. `updated_at` is left alone: the
    /// move is automatic, not a user edit. Returns the updated comments.
    pub async fn apply_anchor_updates(&self, updates: Vec<AnchorUpdate>) -> Result<Vec<CommentId>> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        self.service
            .call("CommentStore::apply_anchor_updates", move |conn| {
                let tx = conn.transaction()?;
                let mut applied = Vec::with_capacity(updates.len());
                {
                    let mut stmt = tx.prepare_cached(
                        "UPDATE comments SET file_path = ?2, old_file_path = ?3, side = ?4,
                             start_line = ?5, end_line = ?6, context_hash = ?7, snippet = ?8,
                             outdated = ?9
                         WHERE id = ?1 AND file_path = ?10 AND old_file_path IS ?11
                             AND side = ?12 AND start_line = ?13 AND end_line = ?14
                             AND context_hash = ?15 AND outdated = ?16",
                    )?;
                    for update in &updates {
                        let (new, old) = (&update.anchor, &update.expected);
                        let changed = stmt.execute(params![
                            update.id,
                            new.file_path,
                            new.old_file_path,
                            new.side,
                            new.start_line,
                            new.end_line,
                            new.context_hash,
                            update.snippet,
                            update.outdated,
                            old.file_path,
                            old.old_file_path,
                            old.side,
                            old.start_line,
                            old.end_line,
                            old.context_hash,
                            update.expected_outdated,
                        ])?;
                        if changed > 0 {
                            applied.push(update.id);
                        }
                    }
                }
                tx.commit()?;
                Ok(applied)
            })
            .await
    }

    /// Re-attaches the comment to a location the user picked: replaces the
    /// anchor, the head SHA it was taken at and the snippet, and clears the
    /// outdated flag.
    pub async fn reattach(
        &self,
        id: CommentId,
        anchor: Anchor,
        anchor_sha: String,
        snippet: String,
    ) -> Result<()> {
        self.service
            .call("CommentStore::reattach", move |conn| {
                let changed = conn.execute(
                    "UPDATE comments SET file_path = ?2, old_file_path = ?3, side = ?4,
                         start_line = ?5, end_line = ?6, context_hash = ?7, anchor_sha = ?8,
                         snippet = ?9, outdated = 0, updated_at = ?10
                     WHERE id = ?1",
                    params![
                        id,
                        anchor.file_path,
                        anchor.old_file_path,
                        anchor.side,
                        anchor.start_line,
                        anchor.end_line,
                        anchor.context_hash,
                        anchor_sha,
                        snippet,
                        now_ms(),
                    ],
                )?;
                expect_changed(changed, "comment", id)
            })
            .await
    }

    /// Flags the comment as outdated: its code is gone from the diff.
    pub async fn mark_outdated(&self, id: CommentId) -> Result<()> {
        self.service
            .call("CommentStore::mark_outdated", move |conn| {
                let changed = conn.execute(
                    "UPDATE comments SET outdated = 1, updated_at = ?2 WHERE id = ?1",
                    params![id, now_ms()],
                )?;
                expect_changed(changed, "comment", id)
            })
            .await
    }

    /// Deletes the comment and, by cascade, its replies. Its runs are kept
    /// with their `comment_id` cleared.
    pub async fn delete(&self, id: CommentId) -> Result<()> {
        self.service
            .call("CommentStore::delete", move |conn| {
                let changed = conn.execute("DELETE FROM comments WHERE id = ?1", [id])?;
                expect_changed(changed, "comment", id)
            })
            .await
    }

    /// Appends a reply to a thread and bumps the thread's `updated_at`.
    pub async fn add_reply(
        &self,
        comment_id: CommentId,
        author: ReplyAuthor,
        body_md: String,
    ) -> Result<Reply> {
        self.insert_reply(comment_id, author, body_md, None).await
    }

    /// Appends Claude's final message of `run_id` to its comment's thread,
    /// linked to the run so the thread can show its model and effort.
    pub async fn add_run_reply(
        &self,
        comment_id: CommentId,
        run_id: RunId,
        body_md: String,
    ) -> Result<Reply> {
        self.insert_reply(comment_id, ReplyAuthor::Claude, body_md, Some(run_id))
            .await
    }

    async fn insert_reply(
        &self,
        comment_id: CommentId,
        author: ReplyAuthor,
        body_md: String,
        run_id: Option<RunId>,
    ) -> Result<Reply> {
        self.service
            .call("CommentStore::add_reply", move |conn| {
                let created_at = now_ms();
                let run = match run_id {
                    Some(id) => conn
                        .query_row(
                            "SELECT model, effort FROM runs WHERE id = ?1",
                            [id],
                            |row| {
                                Ok(ReplyRun {
                                    id,
                                    model: row.get(0)?,
                                    effort: row.get(1)?,
                                })
                            },
                        )
                        .optional()?,
                    None => None,
                };
                let reply = Reply {
                    id: ReplyId::new(),
                    comment_id,
                    author,
                    body_md,
                    created_at,
                    run,
                };
                let tx = conn.transaction()?;
                let changed = tx.execute(
                    "UPDATE comments SET updated_at = ?2 WHERE id = ?1",
                    params![comment_id, reply.created_at],
                )?;
                expect_changed(changed, "comment", comment_id)?;
                tx.execute(
                    "INSERT INTO comment_replies (id, comment_id, author, body_md, created_at,
                         run_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        reply.id,
                        reply.comment_id,
                        reply.author,
                        reply.body_md,
                        reply.created_at,
                        reply.run.as_ref().map(|run| run.id),
                    ],
                )?;
                tx.commit()?;
                Ok(reply)
            })
            .await
    }

    /// Lists the replies of every comment of one (repo, base, head) review,
    /// grouped by comment and oldest first within a thread.
    pub async fn replies_for(
        &self,
        repo_id: RepoId,
        base_ref: String,
        head_ref: String,
    ) -> Result<Vec<Reply>> {
        self.service
            .call("CommentStore::replies_for", move |conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {REPLY_COLUMNS}
                     FROM comment_replies r JOIN comments c ON c.id = r.comment_id
                     LEFT JOIN runs u ON u.id = r.run_id
                     WHERE c.repo_id = ?1 AND c.base_ref = ?2 AND c.head_ref = ?3
                     ORDER BY r.comment_id, r.created_at, r.id"
                ))?;
                let rows = stmt.query_map(params![repo_id, base_ref, head_ref], reply_from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }

    /// Lists a thread's replies, oldest first.
    pub async fn replies(&self, comment_id: CommentId) -> Result<Vec<Reply>> {
        self.service
            .call("CommentStore::replies", move |conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {REPLY_COLUMNS}
                     FROM comment_replies r LEFT JOIN runs u ON u.id = r.run_id
                     WHERE r.comment_id = ?1 ORDER BY r.created_at, r.id"
                ))?;
                let rows = stmt.query_map([comment_id], reply_from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }
}
