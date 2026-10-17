//! Typed errors of the comment model and service.

use argus_store::{CommentId, StoreError};

use crate::review::status::InvalidTransition;

/// Result alias of the review module.
pub type ReviewResult<T, E = ReviewError> = std::result::Result<T, E>;

/// Everything that can go wrong while creating or updating comments.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The commented range is not inside the file on its side.
    #[error("lines {start_line}-{end_line} are not inside the file ({line_count} lines)")]
    InvalidRange {
        /// Requested first line.
        start_line: u32,
        /// Requested last line.
        end_line: u32,
        /// Lines in the file on the anchored side.
        line_count: u32,
    },

    /// The state machine refused the event; nothing was changed.
    #[error(transparent)]
    Transition(#[from] InvalidTransition),

    /// Outdated comments cannot start a run until they are re-anchored
    /// (spec Q5).
    #[error("comment {0} is outdated; re-anchor it before running it")]
    Outdated(CommentId),

    /// The comment's status kept changing under the update.
    #[error("comment {0} changed while its status was being updated; try again")]
    Contended(CommentId),

    /// No (repo, base, head) review is loaded.
    #[error("no review is loaded")]
    NoReview,

    /// Storage failed (including a comment that no longer exists).
    #[error(transparent)]
    Store(#[from] StoreError),
}
