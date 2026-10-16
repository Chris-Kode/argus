//! The `accepts` table: proposals that became commits.

use rusqlite::{Row, params};

use crate::error::Result;
use crate::model::{AcceptId, RunId};
use crate::{StorageService, now_ms};

/// A proposal accepted as a local commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accept {
    /// Identifier.
    pub id: AcceptId,
    /// Run whose proposal was accepted.
    pub run_id: RunId,
    /// SHA of the commit created on the head branch.
    pub commit_sha: String,
    /// Full commit message.
    pub message: String,
    /// Time of the accept, unix ms.
    pub created_at: i64,
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Accept> {
    Ok(Accept {
        id: row.get(0)?,
        run_id: row.get(1)?,
        commit_sha: row.get(2)?,
        message: row.get(3)?,
        created_at: row.get(4)?,
    })
}

/// Async access to `accepts`.
#[derive(Clone, Debug)]
pub struct AcceptStore {
    service: StorageService,
}

impl AcceptStore {
    pub(crate) fn new(service: StorageService) -> Self {
        Self { service }
    }

    /// Records that `run_id`'s proposal landed as `commit_sha`.
    pub async fn record(
        &self,
        run_id: RunId,
        commit_sha: String,
        message: String,
    ) -> Result<Accept> {
        self.service
            .call("AcceptStore::record", move |conn| {
                let accept = Accept {
                    id: AcceptId::new(),
                    run_id,
                    commit_sha,
                    message,
                    created_at: now_ms(),
                };
                conn.execute(
                    "INSERT INTO accepts (id, run_id, commit_sha, message, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        accept.id,
                        accept.run_id,
                        accept.commit_sha,
                        accept.message,
                        accept.created_at
                    ],
                )?;
                Ok(accept)
            })
            .await
    }

    /// Lists the accepts of a run, oldest first.
    pub async fn list_for_run(&self, run_id: RunId) -> Result<Vec<Accept>> {
        self.service
            .call("AcceptStore::list_for_run", move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, run_id, commit_sha, message, created_at FROM accepts
                     WHERE run_id = ?1 ORDER BY created_at, id",
                )?;
                let rows = stmt.query_map([run_id], from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }
}
