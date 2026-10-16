//! Identifiers and enumerations shared by the stores, with their SQLite
//! encodings.

use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a fresh time-ordered (UUID v7) identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID.
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.hyphenated().fmt(f)
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.0.hyphenated().to_string()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                Uuid::parse_str(text)
                    .map(Self)
                    .map_err(|err| FromSqlError::Other(Box::new(err)))
            }
        }
    };
}

uuid_id!(
    /// Identifier of a row in `repos`.
    RepoId
);
uuid_id!(
    /// Identifier of a root comment (thread) in `comments`.
    CommentId
);
uuid_id!(
    /// Identifier of a row in `comment_replies`.
    ReplyId
);
uuid_id!(
    /// Identifier of a Claude run in `runs`.
    RunId
);
uuid_id!(
    /// Identifier of a row in `accepts`.
    AcceptId
);

macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $( $(#[$vmeta])* #[serde(rename = $text)] $variant, )+
        }

        impl $name {
            /// The text stored in SQLite for this value.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text, )+
                }
            }

            /// Parses the stored text form.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $( $text => Some(Self::$variant), )+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                Self::parse(text).ok_or_else(|| {
                    FromSqlError::Other(
                        format!(concat!("unknown ", stringify!($name), " `{}`"), text).into(),
                    )
                })
            }
        }
    };
}

text_enum!(
    /// Which side of the diff a comment is anchored to.
    Side {
        /// The base (merge-base) side.
        Old => "old",
        /// The head side.
        New => "new",
    }
);

text_enum!(
    /// Comment status (spec §7.2).
    CommentStatus {
        /// Waiting for action.
        Open => "open",
        /// A run for this comment is active.
        Running => "running",
        /// A non-empty proposal is waiting for review.
        Proposed => "proposed",
        /// The proposal was accepted.
        Resolved => "resolved",
        /// The user dismissed the comment.
        Dismissed => "dismissed",
    }
);

text_enum!(
    /// Author of a reply in a comment thread.
    ReplyAuthor {
        /// The reviewer.
        User => "user",
        /// Claude (for example a run summary).
        Claude => "claude",
    }
);

text_enum!(
    /// Why a run was started.
    RunKind {
        /// First run for a comment.
        Comment => "comment",
        /// A follow-up resuming the comment's session.
        Followup => "followup",
        /// A conflict-resolution run.
        Resolve => "resolve",
    }
);

text_enum!(
    /// Run lifecycle state (spec §6.3).
    RunStatus {
        /// Created, process not yet spawned.
        Queued => "queued",
        /// The `claude` process is running.
        Running => "running",
        /// Finished with a non-empty proposal.
        Succeeded => "succeeded",
        /// Finished without changing anything.
        NoChanges => "no_changes",
        /// Failed; the reason is stored in `error`.
        Failed => "failed",
        /// Stopped by the user.
        Stopped => "stopped",
    }
);

impl RunStatus {
    /// Whether the run is still in progress.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}
