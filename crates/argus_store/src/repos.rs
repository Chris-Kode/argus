//! The `repos` table: repositories the user has opened.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension as _, Row, params};

use crate::error::Result;
use crate::model::RepoId;
use crate::{StorageService, expect_changed, now_ms, path_to_text};

/// A repository known to Argus.
#[derive(Clone, Debug, PartialEq)]
pub struct Repo {
    /// Stable identifier, also used for the worktree directory.
    pub id: RepoId,
    /// Absolute path of the repository on disk.
    pub path: PathBuf,
    /// Display name (the directory name by default).
    pub name: String,
    /// Creation time, unix ms.
    pub created_at: i64,
    /// Last time the repository was opened, unix ms.
    pub last_opened_at: Option<i64>,
    /// Base branch picked the last time.
    pub last_base: Option<String>,
    /// Head branch picked the last time.
    pub last_head: Option<String>,
}

const COLUMNS: &str = "id, path, name, created_at, last_opened_at, last_base, last_head";

fn from_row(row: &Row<'_>) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: row.get(0)?,
        path: PathBuf::from(row.get::<_, String>(1)?),
        name: row.get(2)?,
        created_at: row.get(3)?,
        last_opened_at: row.get(4)?,
        last_base: row.get(5)?,
        last_head: row.get(6)?,
    })
}

/// Async access to `repos`.
#[derive(Clone, Debug)]
pub struct RepoStore {
    service: StorageService,
}

impl RepoStore {
    pub(crate) fn new(service: StorageService) -> Self {
        Self { service }
    }

    /// Returns the id of the repository at `path`, inserting it if unknown.
    pub async fn upsert(&self, path: &Path) -> Result<RepoId> {
        let text = path_to_text(path)?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| text.clone());
        self.service
            .call("RepoStore::upsert", move |conn| {
                let tx = conn.transaction()?;
                let existing: Option<RepoId> = tx
                    .query_row("SELECT id FROM repos WHERE path = ?1", [&text], |row| {
                        row.get(0)
                    })
                    .optional()?;
                let id = match existing {
                    Some(id) => id,
                    None => {
                        let id = RepoId::new();
                        tx.execute(
                            "INSERT INTO repos (id, path, name, created_at) VALUES (?1, ?2, ?3, ?4)",
                            params![id, text, name, now_ms()],
                        )?;
                        id
                    }
                };
                tx.commit()?;
                Ok(id)
            })
            .await
    }

    /// Loads one repository.
    pub async fn get(&self, id: RepoId) -> Result<Option<Repo>> {
        self.service
            .call("RepoStore::get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {COLUMNS} FROM repos WHERE id = ?1"),
                        [id],
                        from_row,
                    )
                    .optional()?)
            })
            .await
    }

    /// Lists every repository, most recently opened first.
    pub async fn list(&self) -> Result<Vec<Repo>> {
        self.service
            .call("RepoStore::list", move |conn| {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM repos
                     ORDER BY last_opened_at IS NULL, last_opened_at DESC, name"
                ))?;
                let rows = stmt.query_map([], from_row)?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await
    }

    /// Records that the repository was opened now.
    pub async fn touch_last_opened(&self, id: RepoId) -> Result<()> {
        self.service
            .call("RepoStore::touch_last_opened", move |conn| {
                let changed = conn.execute(
                    "UPDATE repos SET last_opened_at = ?2 WHERE id = ?1",
                    params![id, now_ms()],
                )?;
                expect_changed(changed, "repo", id)
            })
            .await
    }

    /// Remembers the base/head branches picked for the repository.
    pub async fn set_last_branches(
        &self,
        id: RepoId,
        base: Option<String>,
        head: Option<String>,
    ) -> Result<()> {
        self.service
            .call("RepoStore::set_last_branches", move |conn| {
                let changed = conn.execute(
                    "UPDATE repos SET last_base = ?2, last_head = ?3 WHERE id = ?1",
                    params![id, base, head],
                )?;
                expect_changed(changed, "repo", id)
            })
            .await
    }

    /// Deletes the repository and, by cascade, its comments.
    pub async fn delete(&self, id: RepoId) -> Result<()> {
        self.service
            .call("RepoStore::delete", move |conn| {
                let changed = conn.execute("DELETE FROM repos WHERE id = ?1", [id])?;
                expect_changed(changed, "repo", id)
            })
            .await
    }
}
