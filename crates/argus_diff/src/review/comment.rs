//! The comment thread model and its mapping to the stored rows (spec §7.1).

use argus_store::{CommentId, CommentStatus, Reply, RepoId};

use crate::model::Side;
use crate::review::anchor::CommentAnchor;

/// The review a comment belongs to. Comments are keyed by (repo, base, head)
/// (spec Q2): switching base hides the comments of the other pair.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReviewKey {
    /// Repository record.
    pub repo_id: RepoId,
    /// Base branch short name, e.g. `main` or `origin/main`.
    pub base_ref: String,
    /// Head branch short name.
    pub head_ref: String,
}

impl ReviewKey {
    /// Builds a key.
    pub fn new(repo_id: RepoId, base_ref: impl Into<String>, head_ref: impl Into<String>) -> Self {
        Self {
            repo_id,
            base_ref: base_ref.into(),
            head_ref: head_ref.into(),
        }
    }

    /// Whether `comment` belongs to this review.
    pub fn is_of(&self, comment: &Comment) -> bool {
        self.repo_id == comment.repo_id
            && self.base_ref == comment.base_ref
            && self.head_ref == comment.head_ref
    }
}

/// A comment thread: the root comment, its anchor and its replies.
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
    /// Where the comment points.
    pub anchor: CommentAnchor,
    /// Markdown body of the root comment.
    pub body_md: String,
    /// Workflow status (spec §7.2).
    pub status: CommentStatus,
    /// Re-anchoring found no match; the anchor shows the original snippet.
    pub outdated: bool,
    /// Replies, oldest first.
    pub replies: Vec<Reply>,
    /// Creation time, unix ms.
    pub created_at: i64,
    /// Last modification time (including new replies), unix ms.
    pub updated_at: i64,
}

impl Comment {
    /// The review the comment belongs to.
    pub fn key(&self) -> ReviewKey {
        ReviewKey::new(self.repo_id, &self.base_ref, &self.head_ref)
    }

    /// Whether the thread is closed (resolved or dismissed).
    pub fn is_closed(&self) -> bool {
        matches!(
            self.status,
            CommentStatus::Resolved | CommentStatus::Dismissed
        )
    }

    pub(crate) fn from_stored(stored: argus_store::Comment, replies: Vec<Reply>) -> Self {
        let argus_store::Comment {
            id,
            repo_id,
            base_ref,
            head_ref,
            anchor,
            anchor_sha,
            snippet,
            body_md,
            status,
            outdated,
            created_at,
            updated_at,
        } = stored;
        Self {
            id,
            repo_id,
            base_ref,
            head_ref,
            anchor: CommentAnchor {
                file_path: anchor.file_path,
                old_file_path: anchor.old_file_path,
                side: side_from_store(anchor.side),
                start_line: anchor.start_line,
                end_line: anchor.end_line,
                anchor_sha,
                context_hash: anchor.context_hash,
                snippet,
            },
            body_md,
            status,
            outdated,
            replies,
            created_at,
            updated_at,
        }
    }
}

/// The location columns of an anchor, as stored.
pub(crate) fn store_anchor(anchor: &CommentAnchor) -> argus_store::Anchor {
    argus_store::Anchor {
        file_path: anchor.file_path.clone(),
        old_file_path: anchor.old_file_path.clone(),
        side: side_to_store(anchor.side),
        start_line: anchor.start_line,
        end_line: anchor.end_line,
        context_hash: anchor.context_hash.clone(),
    }
}

fn side_from_store(side: argus_store::Side) -> Side {
    match side {
        argus_store::Side::Old => Side::Old,
        argus_store::Side::New => Side::New,
    }
}

fn side_to_store(side: Side) -> argus_store::Side {
    match side {
        Side::Old => argus_store::Side::Old,
        Side::New => argus_store::Side::New,
    }
}
