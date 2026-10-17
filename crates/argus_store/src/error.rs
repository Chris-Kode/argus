//! Typed errors returned by the storage service.

use std::path::PathBuf;

/// Errors produced by [`crate::StorageService`] and its repositories.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite reported an error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A settings value could not be encoded or decoded as JSON.
    #[error("invalid JSON value: {0}")]
    Json(#[from] serde_json::Error),

    /// The database directory could not be created.
    #[error("failed to create {}: {source}", path.display())]
    CreateDir {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The database was written by a newer Argus and this build cannot read it.
    #[error(
        "the database at {} uses schema version {found}, but this Argus only supports up to \
         version {supported}; update Argus to open it",
        path.display()
    )]
    NewerSchema {
        /// Location of the database.
        path: PathBuf,
        /// Version recorded in the database.
        found: i64,
        /// Newest version this build knows.
        supported: i64,
    },

    /// A migration left the database inconsistent and was rolled back.
    #[error("storage migration {version} failed: {message}")]
    Migration {
        /// Version the migration would have reached.
        version: i64,
        /// What was wrong.
        message: String,
    },

    /// A path stored in the database must be valid UTF-8.
    #[error("path is not valid UTF-8: {}", .0.display())]
    NonUtf8Path(PathBuf),

    /// The row targeted by an update does not exist.
    #[error("{entity} {id} not found")]
    NotFound {
        /// Table-level name of the entity (`comment`, `run`, ...).
        entity: &'static str,
        /// Identifier that was looked up.
        id: String,
    },

    /// The writer thread is gone (the service was shut down or panicked).
    #[error("the storage service is closed")]
    Closed,
}

/// Result alias used across the crate.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;
