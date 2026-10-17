//! The comment status state machine (spec §6.3, §7.2).
//!
//! ```text
//! Open ──Run──► Running ──success──► Proposed ──Accept──► Resolved
//!   ▲              │                     │
//!   │         fail/stop/empty            ├─Reject──► Open
//!   └──────────────┘                     └─Follow-up──► Running
//! Open/Proposed ──Dismiss──► Dismissed ──Reopen──► Open
//! ```

use std::fmt;

pub use argus_store::CommentStatus;
use argus_store::RunStatus;

/// Something that happened to a comment and may move its status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommentEvent {
    /// A run for the comment started.
    RunStarted,
    /// The run finished with a non-empty proposal.
    RunSucceeded,
    /// The run failed (non-zero exit, error result, or interrupted).
    RunFailed,
    /// The user stopped the run.
    RunStopped,
    /// The run finished without changing anything.
    RunEmpty,
    /// The user accepted the proposal; it is committed.
    Accepted,
    /// The user rejected the proposal; its worktree is deleted.
    Rejected,
    /// The user replied to the proposal, resuming the session.
    FollowUp,
    /// The user dismissed the comment.
    Dismissed,
    /// The user reopened a dismissed comment.
    Reopened,
}

impl CommentEvent {
    /// The event a finished run produces, from its final status. `None` for
    /// a run still queued or running, and for a rejected one (Reject applies
    /// [`CommentEvent::Rejected`] itself).
    pub fn from_run_status(status: RunStatus) -> Option<Self> {
        match status {
            RunStatus::Queued | RunStatus::Running | RunStatus::Rejected => None,
            RunStatus::Succeeded => Some(Self::RunSucceeded),
            RunStatus::NoChanges => Some(Self::RunEmpty),
            RunStatus::Failed => Some(Self::RunFailed),
            RunStatus::Stopped => Some(Self::RunStopped),
        }
    }

    /// Lowercase label for logs and error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run started",
            Self::RunSucceeded => "run succeeded",
            Self::RunFailed => "run failed",
            Self::RunStopped => "run stopped",
            Self::RunEmpty => "run empty",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::FollowUp => "follow-up",
            Self::Dismissed => "dismissed",
            Self::Reopened => "reopened",
        }
    }
}

impl fmt::Display for CommentEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `event` is not allowed while the comment is in `from`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot apply `{event}` to a comment that is {from}")]
pub struct InvalidTransition {
    /// Status the comment was in.
    pub from: CommentStatus,
    /// Event that was refused.
    pub event: CommentEvent,
}

/// The status a comment in `status` moves to on `event`, or an error when
/// the state machine has no such edge. Pure: callers apply the result only
/// on `Ok`.
pub fn transition(
    status: CommentStatus,
    event: CommentEvent,
) -> Result<CommentStatus, InvalidTransition> {
    use CommentEvent as E;
    use CommentStatus as S;
    let next = match (status, event) {
        (S::Open, E::RunStarted) => S::Running,
        (S::Running, E::RunSucceeded) => S::Proposed,
        (S::Running, E::RunFailed | E::RunStopped | E::RunEmpty) => S::Open,
        (S::Proposed, E::Accepted) => S::Resolved,
        (S::Proposed, E::Rejected) => S::Open,
        (S::Proposed, E::FollowUp) => S::Running,
        (S::Open | S::Proposed, E::Dismissed) => S::Dismissed,
        (S::Dismissed, E::Reopened) => S::Open,
        (from, event) => return Err(InvalidTransition { from, event }),
    };
    Ok(next)
}
