//! The `comments` and `comment_replies` tables.

use rusqlite::{OptionalExtension as _, Row, params};

use crate::error::Result;
use crate::model::{CommentId, CommentStatus, ReplyAuthor, ReplyId, RepoId, Side};
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
}

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

    /// Sets the workflow status. Transition rules (spec §7.2) are enforced by
    /// the caller.
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
        self.service
            .call("CommentStore::add_reply", move |conn| {
                let reply = Reply {
                    id: ReplyId::new(),
                    comment_id,
                    author,
                    body_md,
                    created_at: now_ms(),
                };
                let tx = conn.transaction()?;
                let changed = tx.execute(
                    "UPDATE comments SET updated_at = ?2 WHERE id = ?1",
                    params![comment_id, reply.created_at],
                )?;
                expect_changed(changed, "comment", comment_id)?;
                tx.execute(
                    "INSERT INTO comment_replies (id, comment_id, author, body_md, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        reply.id,
                        reply.comment_id,
                        reply.author,
                        reply.body_md,
                        reply.created_at
                    ],
                )?;
                tx.commit()?;
                Ok(reply)
            })
            .await
    }

    /// Lists a thread's replies, oldest first.
    pub async fn replies(&self, comment_id: CommentId) -> Result<Vec<Reply>> {
        self.service
            .call("CommentStore::replies", move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, comment_id, author, body_md, created_at FROM comment_replies
                     WHERE comment_id = ?1 ORDER BY created_at, id",
                )?;
                let rows = stmt.query_map([comment_id], reply_from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }
}
