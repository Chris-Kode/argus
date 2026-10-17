//! Review comments: anchors, the status state machine and the service that
//! keeps the open review's comments (spec §7).
//!
//! - [`CommentAnchor`] ties a comment to a file, side and line range, with a
//!   whitespace-normalized [`context_hash`] and the original snippet for when
//!   the code is gone.
//! - [`transition`] is the pure status state machine of spec §7.2; nothing
//!   is persisted unless it accepts the event.
//! - [`CommentService`] holds the comments of one (repo, base, head) review,
//!   persists every change through [`argus_store::CommentStore`] and
//!   broadcasts [`CommentChange`]s for the UI.

mod anchor;
mod comment;
mod error;
mod service;
mod status;

pub use anchor::{
    AnchorLocation, CONTEXT_LINES, CommentAnchor, context_hash, context_hash_at, context_window,
};
pub use argus_store::{CommentId, Reply, ReplyAuthor, ReplyId, RepoId};
pub use comment::{Comment, ReviewKey};
pub use error::{ReviewError, ReviewResult};
pub use service::{CommentChange, CommentService, apply_event};
pub use status::{CommentEvent, CommentStatus, InvalidTransition, transition};
