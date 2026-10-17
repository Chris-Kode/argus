//! The dedicated storage thread: it owns the only SQLite connection and
//! executes jobs sent over a channel, batching run events.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use tokio::sync::oneshot;

use crate::error::{Result, StoreError};
use crate::migrations;
use crate::model::RunId;

/// Flush buffered run events this long after the first one arrived.
const EVENT_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// Flush buffered run events once this many are waiting.
const EVENT_FLUSH_COUNT: usize = 100;

/// A unit of work executed on the storage thread.
pub(crate) type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// One run event waiting to be inserted.
pub(crate) struct PendingEvent {
    pub run_id: RunId,
    pub seq: i64,
    pub ts: i64,
    pub event_type: String,
    pub json: String,
}

/// Messages understood by the storage thread.
pub(crate) enum Command {
    /// Run a closure against the connection. Pending events are flushed first,
    /// so every job observes all events appended before it.
    Job(Job),
    /// Buffer a run event for the next batched insert.
    Event(PendingEvent),
}

/// Opens (creating if needed) the database at `path`, runs migrations and
/// starts the storage thread. Returns once the database is ready.
pub(crate) async fn spawn(path: PathBuf) -> Result<mpsc::Sender<Command>> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::CreateDir {
            path: dir.to_path_buf(),
            source,
        })?;
    }

    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = oneshot::channel();

    thread::Builder::new()
        .name("argus-store".into())
        .spawn(move || match open(&path) {
            Ok(conn) => {
                let _ = ready_tx.send(Ok(()));
                run_loop(conn, rx);
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err));
            }
        })
        .map_err(|source| StoreError::CreateDir {
            path: PathBuf::from("<storage thread>"),
            source,
        })?;

    ready_rx.await.map_err(|_| StoreError::Closed)??;
    Ok(tx)
}

fn open(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    // `journal_mode` returns the resulting mode as a row.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(%mode, "SQLite refused WAL mode");
    }
    conn.pragma_update(None, "foreign_keys", true)?;
    // NORMAL is durable across application crashes in WAL mode and much
    // cheaper than FULL for the event stream.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    migrations::run(&mut conn, path)?;
    tracing::info!(path = %path.display(), "storage opened");
    Ok(conn)
}

fn run_loop(mut conn: Connection, rx: Receiver<Command>) {
    let mut pending: Vec<PendingEvent> = Vec::with_capacity(EVENT_FLUSH_COUNT);
    let mut deadline: Option<Instant> = None;

    loop {
        let message = match deadline {
            Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };

        match message {
            Ok(Command::Event(event)) => {
                pending.push(event);
                deadline.get_or_insert_with(|| Instant::now() + EVENT_FLUSH_INTERVAL);
                if pending.len() >= EVENT_FLUSH_COUNT {
                    flush_events(&mut conn, &mut pending);
                    deadline = None;
                }
            }
            Ok(Command::Job(job)) => {
                if !pending.is_empty() {
                    flush_events(&mut conn, &mut pending);
                    deadline = None;
                }
                job(&mut conn);
            }
            Err(RecvTimeoutError::Timeout) => {
                flush_events(&mut conn, &mut pending);
                deadline = None;
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush_events(&mut conn, &mut pending);
                tracing::debug!("storage thread exiting");
                return;
            }
        }
    }
}

/// Inserts every buffered event in one transaction. A bad row (for example an
/// unknown run) is logged and skipped so it cannot drop the rest of the batch.
fn flush_events(conn: &mut Connection, pending: &mut Vec<PendingEvent>) {
    if pending.is_empty() {
        return;
    }
    let started = Instant::now();
    let count = pending.len();

    let result = (|| -> Result<()> {
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO run_events (run_id, seq, ts, type, json) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for event in pending.drain(..) {
                if let Err(err) = stmt.execute(params![
                    event.run_id,
                    event.seq,
                    event.ts,
                    event.event_type,
                    event.json
                ]) {
                    tracing::warn!(run = %event.run_id, seq = event.seq, %err, "dropped run event");
                }
            }
        }
        tx.commit()?;
        Ok(())
    })();

    pending.clear();
    match result {
        Ok(()) => tracing::trace!(count, elapsed = ?started.elapsed(), "flushed run events"),
        Err(err) => tracing::error!(count, %err, "failed to flush run events"),
    }
}
