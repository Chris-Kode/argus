//! SQLite storage for Argus, kept outside every repo at `~/.argus/argus.db`.
//!
//! Filled in by issue #3: embedded migrations plus the repo, comment, run and
//! settings repositories. All access happens off the UI thread (a dedicated
//! writer thread or `spawn_blocking`), exposed as async functions.

/// Handle to the storage service.
///
/// Placeholder until issue #3 lands: it will own the connection and the async
/// repository API used by views and services.
#[derive(Clone, Debug, Default)]
pub struct StorageService;

impl StorageService {
    /// Creates the service handle.
    pub fn new() -> Self {
        Self
    }
}
