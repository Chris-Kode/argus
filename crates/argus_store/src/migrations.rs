//! Embedded, ordered schema migrations tracked in a `schema_version` table.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::error::{Result, StoreError};

/// Every migration, in order. Migration `i` moves the schema from version `i`
/// to version `i + 1`. Never edit a released entry; append a new one instead.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema (spec §8). IDs are UUID v7 text, timestamps unix ms.
    r#"
    CREATE TABLE repos (
        id             TEXT PRIMARY KEY NOT NULL,
        path           TEXT NOT NULL UNIQUE,
        name           TEXT NOT NULL,
        created_at     INTEGER NOT NULL,
        last_opened_at INTEGER,
        last_base      TEXT,
        last_head      TEXT
    );

    CREATE TABLE comments (
        id             TEXT PRIMARY KEY NOT NULL,
        repo_id        TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        base_ref       TEXT NOT NULL,
        head_ref       TEXT NOT NULL,
        file_path      TEXT NOT NULL,
        old_file_path  TEXT,
        side           TEXT NOT NULL CHECK (side IN ('old', 'new')),
        start_line     INTEGER NOT NULL,
        end_line       INTEGER NOT NULL,
        anchor_sha     TEXT NOT NULL,
        context_hash   TEXT NOT NULL,
        snippet        TEXT NOT NULL,
        body_md        TEXT NOT NULL,
        status         TEXT NOT NULL CHECK (status IN
                           ('open', 'running', 'proposed', 'resolved', 'dismissed')),
        outdated       INTEGER NOT NULL DEFAULT 0 CHECK (outdated IN (0, 1)),
        created_at     INTEGER NOT NULL,
        updated_at     INTEGER NOT NULL,
        CHECK (start_line >= 1 AND end_line >= start_line)
    );
    CREATE INDEX comments_repo_refs ON comments(repo_id, base_ref, head_ref);

    CREATE TABLE comment_replies (
        id          TEXT PRIMARY KEY NOT NULL,
        comment_id  TEXT NOT NULL REFERENCES comments(id) ON DELETE CASCADE,
        author      TEXT NOT NULL CHECK (author IN ('user', 'claude')),
        body_md     TEXT NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX comment_replies_comment ON comment_replies(comment_id);

    -- A run outlives its comment (SET NULL) so accepted commits keep their
    -- audit trail; resolve runs may have no comment at all.
    CREATE TABLE runs (
        id             TEXT PRIMARY KEY NOT NULL,
        comment_id     TEXT REFERENCES comments(id) ON DELETE SET NULL,
        kind           TEXT NOT NULL CHECK (kind IN ('comment', 'followup', 'resolve')),
        session_id     TEXT NOT NULL,
        model          TEXT NOT NULL,
        effort         TEXT NOT NULL,
        worktree_path  TEXT NOT NULL,
        start_sha      TEXT NOT NULL,
        prompt         TEXT NOT NULL,
        status         TEXT NOT NULL CHECK (status IN
                           ('queued', 'running', 'succeeded', 'no_changes', 'failed', 'stopped')),
        exit_code      INTEGER,
        cost_usd       REAL,
        num_turns      INTEGER,
        duration_ms    INTEGER,
        summary        TEXT,
        error          TEXT,
        created_at     INTEGER NOT NULL,
        finished_at    INTEGER
    );
    CREATE INDEX runs_comment ON runs(comment_id);
    CREATE INDEX runs_status ON runs(status);

    -- Append-only and hot (one row per stream-json line), so it keys on the
    -- rowid instead of a UUID.
    CREATE TABLE run_events (
        id      INTEGER PRIMARY KEY,
        run_id  TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
        seq     INTEGER NOT NULL,
        ts      INTEGER NOT NULL,
        type    TEXT NOT NULL,
        json    TEXT NOT NULL
    );
    CREATE INDEX run_events_run_seq ON run_events(run_id, seq);

    CREATE TABLE accepts (
        id          TEXT PRIMARY KEY NOT NULL,
        run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
        commit_sha  TEXT NOT NULL,
        message     TEXT NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX accepts_run ON accepts(run_id);

    CREATE TABLE settings (
        key         TEXT PRIMARY KEY NOT NULL,
        value_json  TEXT NOT NULL
    );
    "#,
    // 2: runs can be `rejected` (the user rejected the proposal, #22). SQLite
    // cannot alter a CHECK constraint, so the table is rebuilt; `run` turns
    // foreign keys off around migrations, so dropping the old table does not
    // cascade into `run_events` and `accepts`.
    r#"
    CREATE TABLE runs_new (
        id             TEXT PRIMARY KEY NOT NULL,
        comment_id     TEXT REFERENCES comments(id) ON DELETE SET NULL,
        kind           TEXT NOT NULL CHECK (kind IN ('comment', 'followup', 'resolve')),
        session_id     TEXT NOT NULL,
        model          TEXT NOT NULL,
        effort         TEXT NOT NULL,
        worktree_path  TEXT NOT NULL,
        start_sha      TEXT NOT NULL,
        prompt         TEXT NOT NULL,
        status         TEXT NOT NULL CHECK (status IN
                           ('queued', 'running', 'succeeded', 'no_changes', 'failed', 'stopped',
                            'rejected')),
        exit_code      INTEGER,
        cost_usd       REAL,
        num_turns      INTEGER,
        duration_ms    INTEGER,
        summary        TEXT,
        error          TEXT,
        created_at     INTEGER NOT NULL,
        finished_at    INTEGER
    );
    INSERT INTO runs_new (id, comment_id, kind, session_id, model, effort, worktree_path,
                          start_sha, prompt, status, exit_code, cost_usd, num_turns,
                          duration_ms, summary, error, created_at, finished_at)
        SELECT id, comment_id, kind, session_id, model, effort, worktree_path, start_sha,
               prompt, status, exit_code, cost_usd, num_turns, duration_ms, summary, error,
               created_at, finished_at
        FROM runs;
    DROP TABLE runs;
    ALTER TABLE runs_new RENAME TO runs;
    CREATE INDEX runs_comment ON runs(comment_id);
    CREATE INDEX runs_status ON runs(status);
    "#,
];

/// Newest schema version this build understands.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Brings the database up to [`SCHEMA_VERSION`].
///
/// Each migration runs in its own transaction together with the version bump,
/// so a crash leaves the database at a well-defined version. A database at a
/// newer version than this build knows is refused untouched.
pub fn run(conn: &mut Connection, path: &Path) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
             id      INTEGER PRIMARY KEY CHECK (id = 1),
             version INTEGER NOT NULL
         );",
    )?;

    let current: i64 = conn
        .query_row(
            "SELECT version FROM schema_version WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);

    if current > SCHEMA_VERSION {
        return Err(StoreError::NewerSchema {
            path: path.to_path_buf(),
            found: current,
            supported: SCHEMA_VERSION,
        });
    }

    if current == SCHEMA_VERSION {
        return Ok(());
    }

    // Table rebuilds drop the old table; with foreign keys on, that would
    // cascade-delete its children. The pragma is a no-op inside a
    // transaction, so it is switched around the whole loop, and each
    // migration checks the keys itself before it commits.
    let enforced: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = apply(conn, current);
    conn.pragma_update(None, "foreign_keys", enforced)?;
    result
}

fn apply(conn: &mut Connection, current: i64) -> Result<()> {
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = index as i64 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        let violations: i64 =
            tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations > 0 {
            return Err(StoreError::Migration {
                version,
                message: format!("{violations} foreign key violations"),
            });
        }
        tx.execute(
            "INSERT INTO schema_version (id, version) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET version = excluded.version",
            params![version],
        )?;
        tx.commit()?;
        tracing::info!(version, "applied storage migration");
    }
    Ok(())
}
