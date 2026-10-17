//! The `runs` and `run_events` tables.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension as _, Row, params};

use crate::error::Result;
use crate::model::{CommentId, RepoId, RunId, RunKind, RunStatus};
use crate::writer::PendingEvent;
use crate::{StorageService, expect_changed, now_ms, path_to_text};

/// Fields needed to record a new run.
#[derive(Clone, Debug, PartialEq)]
pub struct NewRun {
    /// Comment the run works on, if any.
    pub comment_id: Option<CommentId>,
    /// Why the run was started.
    pub kind: RunKind,
    /// Claude session UUID shared by a comment thread.
    pub session_id: String,
    /// Model passed to `--model`.
    pub model: String,
    /// Effort passed to `--effort`.
    pub effort: String,
    /// Worktree the run executes in.
    pub worktree_path: PathBuf,
    /// Head SHA the run starts from.
    pub start_sha: String,
    /// Rendered prompt, stored for auditing.
    pub prompt: String,
}

/// A stored run.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// Identifier.
    pub id: RunId,
    /// Comment the run works on, if any.
    pub comment_id: Option<CommentId>,
    /// Why the run was started.
    pub kind: RunKind,
    /// Claude session UUID.
    pub session_id: String,
    /// Model used.
    pub model: String,
    /// Thinking effort used.
    pub effort: String,
    /// Worktree the run executes in.
    pub worktree_path: PathBuf,
    /// Head SHA the run starts from.
    pub start_sha: String,
    /// Rendered prompt.
    pub prompt: String,
    /// Lifecycle state.
    pub status: RunStatus,
    /// Process exit code once finished.
    pub exit_code: Option<i32>,
    /// `total_cost_usd` from the `result` event.
    pub cost_usd: Option<f64>,
    /// Number of turns reported by Claude.
    pub num_turns: Option<u32>,
    /// Wall-clock duration.
    pub duration_ms: Option<i64>,
    /// Claude's final summary.
    pub summary: Option<String>,
    /// Failure reason (stderr tail, `interrupted`, ...).
    pub error: Option<String>,
    /// Creation time, unix ms.
    pub created_at: i64,
    /// Completion time, unix ms.
    pub finished_at: Option<i64>,
}

/// Final fields written by [`RunStore::finish`].
#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    /// Terminal status.
    pub status: RunStatus,
    /// Process exit code.
    pub exit_code: Option<i32>,
    /// Total cost in USD.
    pub cost_usd: Option<f64>,
    /// Number of turns.
    pub num_turns: Option<u32>,
    /// Wall-clock duration.
    pub duration_ms: Option<i64>,
    /// Claude's final summary.
    pub summary: Option<String>,
    /// Failure reason.
    pub error: Option<String>,
}

impl RunOutcome {
    /// An outcome with only the terminal status filled in.
    pub fn new(status: RunStatus) -> Self {
        Self {
            status,
            exit_code: None,
            cost_usd: None,
            num_turns: None,
            duration_ms: None,
            summary: None,
            error: None,
        }
    }
}

/// One logged stream-json line of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunEvent {
    /// Row id, increasing with insertion order.
    pub id: i64,
    /// Run the event belongs to.
    pub run_id: RunId,
    /// Position of the line in the run's stream.
    pub seq: i64,
    /// Time the event was appended, unix ms.
    pub ts: i64,
    /// Event type (`system/init`, `assistant`, `result`, ...).
    pub event_type: String,
    /// The raw JSON line.
    pub json: String,
}

/// A run with the comment it works on, as the run panel lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunListing {
    /// The run.
    pub run: Run,
    /// File path of the comment, in head.
    pub file_path: String,
    /// First line of the comment's range.
    pub start_line: u32,
    /// Last line of the comment's range.
    pub end_line: u32,
    /// The comment's Markdown body.
    pub body_md: String,
}

const COLUMNS: &str = "id, comment_id, kind, session_id, model, effort, worktree_path, \
     start_sha, prompt, status, exit_code, cost_usd, num_turns, duration_ms, summary, error, \
     created_at, finished_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get(0)?,
        comment_id: row.get(1)?,
        kind: row.get(2)?,
        session_id: row.get(3)?,
        model: row.get(4)?,
        effort: row.get(5)?,
        worktree_path: PathBuf::from(row.get::<_, String>(6)?),
        start_sha: row.get(7)?,
        prompt: row.get(8)?,
        status: row.get(9)?,
        exit_code: row.get(10)?,
        cost_usd: row.get(11)?,
        num_turns: row.get(12)?,
        duration_ms: row.get(13)?,
        summary: row.get(14)?,
        error: row.get(15)?,
        created_at: row.get(16)?,
        finished_at: row.get(17)?,
    })
}

/// Async access to `runs` and `run_events`.
#[derive(Clone, Debug)]
pub struct RunStore {
    service: StorageService,
}

impl RunStore {
    pub(crate) fn new(service: StorageService) -> Self {
        Self { service }
    }

    /// Records a new run in the `Queued` state.
    pub async fn create(&self, new: NewRun) -> Result<Run> {
        self.create_with_id(RunId::new(), new).await
    }

    /// Records a new run in the `Queued` state under an id chosen by the
    /// caller, e.g. because the worktree path is derived from it.
    pub async fn create_with_id(&self, id: RunId, new: NewRun) -> Result<Run> {
        let worktree_text = path_to_text(&new.worktree_path)?;
        self.service
            .call("RunStore::create", move |conn| {
                let run = Run {
                    id,
                    comment_id: new.comment_id,
                    kind: new.kind,
                    session_id: new.session_id,
                    model: new.model,
                    effort: new.effort,
                    worktree_path: new.worktree_path,
                    start_sha: new.start_sha,
                    prompt: new.prompt,
                    status: RunStatus::Queued,
                    exit_code: None,
                    cost_usd: None,
                    num_turns: None,
                    duration_ms: None,
                    summary: None,
                    error: None,
                    created_at: now_ms(),
                    finished_at: None,
                };
                conn.execute(
                    "INSERT INTO runs (id, comment_id, kind, session_id, model, effort,
                         worktree_path, start_sha, prompt, status, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        run.id,
                        run.comment_id,
                        run.kind,
                        run.session_id,
                        run.model,
                        run.effort,
                        worktree_text,
                        run.start_sha,
                        run.prompt,
                        run.status,
                        run.created_at,
                    ],
                )?;
                Ok(run)
            })
            .await
    }

    /// Loads one run.
    pub async fn get(&self, id: RunId) -> Result<Option<Run>> {
        self.service
            .call("RunStore::get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {COLUMNS} FROM runs WHERE id = ?1"),
                        [id],
                        from_row,
                    )
                    .optional()?)
            })
            .await
    }

    /// Updates the lifecycle state without finishing the run.
    pub async fn set_status(&self, id: RunId, status: RunStatus) -> Result<()> {
        self.service
            .call("RunStore::set_status", move |conn| {
                let changed = conn.execute(
                    "UPDATE runs SET status = ?2 WHERE id = ?1",
                    params![id, status],
                )?;
                expect_changed(changed, "run", id)
            })
            .await
    }

    /// Writes the outcome of a run and stamps `finished_at`.
    pub async fn finish(&self, id: RunId, outcome: RunOutcome) -> Result<()> {
        let status = outcome.status;
        debug_assert!(!status.is_active(), "finish() needs a terminal status");
        self.service
            .call("RunStore::finish", move |conn| {
                let changed = conn.execute(
                    "UPDATE runs SET status = ?2, exit_code = ?3, cost_usd = ?4, num_turns = ?5,
                         duration_ms = ?6, summary = ?7, error = ?8, finished_at = ?9
                     WHERE id = ?1",
                    params![
                        id,
                        status,
                        outcome.exit_code,
                        outcome.cost_usd,
                        outcome.num_turns,
                        outcome.duration_ms,
                        outcome.summary,
                        outcome.error,
                        now_ms(),
                    ],
                )?;
                expect_changed(changed, "run", id)
            })
            .await
    }

    /// Queues one stream-json line for the run's event log.
    ///
    /// Returns as soon as the event is queued: events are inserted in batches
    /// (every ~50 ms or 100 events) on the storage thread. Any later call on
    /// the service observes the event; [`RunStore::flush_events`] waits for it
    /// explicitly. `seq` is the line's position in the run's stream.
    pub fn append_event(
        &self,
        run_id: RunId,
        seq: i64,
        event_type: impl Into<String>,
        json: impl Into<String>,
    ) -> Result<()> {
        self.service.enqueue_event(PendingEvent {
            run_id,
            seq,
            ts: now_ms(),
            event_type: event_type.into(),
            json: json.into(),
        })
    }

    /// Waits until every event appended so far is written.
    pub async fn flush_events(&self) -> Result<()> {
        // Jobs flush pending events before they run, so an empty job is a barrier.
        self.service
            .call("RunStore::flush_events", |_| Ok(()))
            .await
    }

    /// Loads a run's event log in stream order, for replay.
    pub async fn events(&self, run_id: RunId) -> Result<Vec<RunEvent>> {
        self.service
            .call("RunStore::events", move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, run_id, seq, ts, type, json FROM run_events
                     WHERE run_id = ?1 ORDER BY seq, id",
                )?;
                let rows = stmt.query_map([run_id], |row| {
                    Ok(RunEvent {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        seq: row.get(2)?,
                        ts: row.get(3)?,
                        event_type: row.get(4)?,
                        json: row.get(5)?,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }

    /// Lists runs that are queued or running, oldest first. On startup these
    /// are the runs interrupted by a crash.
    pub async fn list_active(&self) -> Result<Vec<Run>> {
        self.service
            .call("RunStore::list_active", move |conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM runs WHERE status IN ('queued', 'running')
                     ORDER BY created_at, id"
                ))?;
                let rows = stmt.query_map([], from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }

    /// Lists the newest `limit` comment runs of a repository, newest first,
    /// with their comment's location and body. Runs whose comment was
    /// deleted are left out: nothing ties them to a repository any more.
    pub async fn list_recent_for_repo(
        &self,
        repo_id: RepoId,
        limit: usize,
    ) -> Result<Vec<RunListing>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.service
            .call("RunStore::list_recent_for_repo", move |conn| {
                let columns = COLUMNS
                    .split(", ")
                    .map(|column| format!("r.{}", column.trim()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {columns}, c.file_path, c.start_line, c.end_line, c.body_md
                     FROM runs r JOIN comments c ON c.id = r.comment_id
                     WHERE c.repo_id = ?1
                     ORDER BY r.created_at DESC, r.id DESC
                     LIMIT ?2"
                ))?;
                let rows = stmt.query_map(params![repo_id, limit], |row| {
                    Ok(RunListing {
                        run: from_row(row)?,
                        file_path: row.get(18)?,
                        start_line: row.get(19)?,
                        end_line: row.get(20)?,
                        body_md: row.get(21)?,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }

    /// Lists a comment's runs, oldest first.
    pub async fn list_for_comment(&self, comment_id: CommentId) -> Result<Vec<Run>> {
        self.service
            .call("RunStore::list_for_comment", move |conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM runs WHERE comment_id = ?1 ORDER BY created_at, id"
                ))?;
                let rows = stmt.query_map([comment_id], from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }

    /// Lists runs whose worktree lives at `path` (used when reconciling
    /// worktrees on disk).
    pub async fn find_by_worktree(&self, path: &Path) -> Result<Vec<Run>> {
        let text = path_to_text(path)?;
        self.service
            .call("RunStore::find_by_worktree", move |conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM runs WHERE worktree_path = ?1 ORDER BY created_at, id"
                ))?;
                let rows = stmt.query_map([text], from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }

    /// Deletes the run and, by cascade, its events and accept records.
    pub async fn delete(&self, id: RunId) -> Result<()> {
        self.service
            .call("RunStore::delete", move |conn| {
                let changed = conn.execute("DELETE FROM runs WHERE id = ?1", [id])?;
                expect_changed(changed, "run", id)
            })
            .await
    }
}
