//! SQLite storage for Argus, kept outside every repo at `~/.argus/argus.db`
//! (spec §8).
//!
//! A single dedicated thread (`argus-store`) owns the only connection. Callers
//! talk to it through [`StorageService`], whose repositories expose async
//! functions: each call sends a closure over a channel and awaits the reply,
//! so no SQLite work ever happens on the caller's thread. Run events take a
//! cheaper path: they are buffered and inserted in one transaction every
//! ~50 ms or 100 events.
//!
//! The database runs in WAL mode with `foreign_keys=ON`. Embedded migrations
//! run when the service opens; a database written by a newer schema is
//! refused with [`StoreError::NewerSchema`].

mod accepts;
mod comments;
mod error;
mod migrations;
mod model;
mod repos;
mod runs;
mod settings;
mod writer;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use tokio::sync::oneshot;

pub use accepts::{Accept, AcceptStore};
pub use comments::{Anchor, AnchorUpdate, Comment, CommentStore, NewComment, Reply};
pub use error::{Result, StoreError};
pub use migrations::SCHEMA_VERSION;
pub use model::{
    AcceptId, CommentId, CommentStatus, ReplyAuthor, ReplyId, RepoId, RunId, RunKind, RunStatus,
    Side,
};
pub use repos::{Repo, RepoStore};
pub use runs::{NewRun, Run, RunEvent, RunListing, RunOutcome, RunStore};
pub use settings::SettingsStore;

/// File name of the database inside the Argus home directory.
pub const DATABASE_FILE: &str = "argus.db";

/// Resolves the database path inside an Argus home directory
/// (`<argus_home>/argus.db`).
pub fn database_path(argus_home: &Path) -> PathBuf {
    argus_home.join(DATABASE_FILE)
}

thread_local! {
    static IS_UI_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as the GPUI main thread.
///
/// In debug builds every storage call made on a marked thread panics, which
/// enforces the rule that the UI thread never touches SQLite (spec §4.2).
/// Callers must run storage futures on the tokio runtime instead.
pub fn mark_ui_thread() {
    IS_UI_THREAD.with(|flag| flag.set(true));
}

/// Debug-only guard: panics if `operation` runs on the thread marked by
/// [`mark_ui_thread`].
#[inline]
fn assert_off_ui_thread(operation: &'static str) {
    if cfg!(debug_assertions) && IS_UI_THREAD.with(Cell::get) {
        panic!(
            "storage call `{operation}` made on the GPUI main thread; run it on the tokio runtime"
        );
    }
}

/// Current time as unix milliseconds, the timestamp format of every table.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Handle to the storage service. Cheap to clone; every clone talks to the
/// same storage thread, which exits once the last handle is dropped.
#[derive(Clone, Debug)]
pub struct StorageService {
    tx: mpsc::Sender<writer::Command>,
}

impl StorageService {
    /// Opens the database at `path`, creating the file and its directory if
    /// needed, and runs pending migrations.
    ///
    /// Fails with [`StoreError::NewerSchema`] if the database was written by a
    /// newer Argus.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        assert_off_ui_thread("open");
        let tx = writer::spawn(path.into()).await?;
        Ok(Self { tx })
    }

    /// Opens `<argus_home>/argus.db`. See [`StorageService::open`].
    pub async fn open_in(argus_home: &Path) -> Result<Self> {
        Self::open(database_path(argus_home)).await
    }

    /// Repository records (`repos`).
    pub fn repos(&self) -> RepoStore {
        RepoStore::new(self.clone())
    }

    /// Comment threads and replies (`comments`, `comment_replies`).
    pub fn comments(&self) -> CommentStore {
        CommentStore::new(self.clone())
    }

    /// Runs and their event log (`runs`, `run_events`).
    pub fn runs(&self) -> RunStore {
        RunStore::new(self.clone())
    }

    /// Accepted proposals (`accepts`).
    pub fn accepts(&self) -> AcceptStore {
        AcceptStore::new(self.clone())
    }

    /// JSON key/value settings (`settings`).
    pub fn settings(&self) -> SettingsStore {
        SettingsStore::new(self.clone())
    }

    /// Runs `f` on the storage thread and awaits its result.
    ///
    /// Dropping the returned future cancels the wait, not the job: a write that
    /// was already queued still completes, which keeps the database consistent.
    pub(crate) async fn call<T, F>(&self, operation: &'static str, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        assert_off_ui_thread(operation);
        let (reply_tx, reply_rx) = oneshot::channel();
        let job: writer::Job = Box::new(move |conn| {
            let _ = reply_tx.send(f(conn));
        });
        self.tx
            .send(writer::Command::Job(job))
            .map_err(|_| StoreError::Closed)?;
        reply_rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Queues a run event for the next batched insert without waiting.
    pub(crate) fn enqueue_event(&self, event: writer::PendingEvent) -> Result<()> {
        assert_off_ui_thread("append_event");
        self.tx
            .send(writer::Command::Event(event))
            .map_err(|_| StoreError::Closed)
    }
}

/// Maps "zero rows changed" to [`StoreError::NotFound`].
pub(crate) fn expect_changed(
    changed: usize,
    entity: &'static str,
    id: impl ToString,
) -> Result<()> {
    if changed == 0 {
        Err(StoreError::NotFound {
            entity,
            id: id.to_string(),
        })
    } else {
        Ok(())
    }
}

/// Converts a path to the UTF-8 text stored in SQLite.
pub(crate) fn path_to_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| StoreError::NonUtf8Path(path.to_path_buf()))
}
