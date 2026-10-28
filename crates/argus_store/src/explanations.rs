//! The `explanations` table: Claude's explanations of a diff, or of lines
//! in it, cached by what they explain.
//!
//! A row is keyed by the exact commits it explains (merge-base and head
//! tip) and its scope, so once a branch moves the old row no longer
//! matches and a new explanation is needed. Each repository keeps its
//! [`MAX_EXPLANATIONS_PER_REPO`] newest rows.

use rusqlite::{OptionalExtension as _, Row, params};

use crate::error::Result;
use crate::model::RepoId;
use crate::{StorageService, now_ms};

/// Rows kept per repository; older ones are deleted when a new one lands.
pub const MAX_EXPLANATIONS_PER_REPO: usize = 200;

/// What an explanation explains.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExplanationKey {
    /// The reviewed repository.
    pub repo_id: RepoId,
    /// Merge-base SHA of the diff.
    pub merge_base: String,
    /// Head tip SHA of the diff.
    pub head_tip: String,
    /// `all` for the whole diff, or a description of the explained lines.
    pub scope: String,
}

/// A cached explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explanation {
    /// What it explains.
    pub key: ExplanationKey,
    /// The explanation, Markdown.
    pub body_md: String,
    /// Model that wrote it.
    pub model: String,
    /// Thinking effort it was written with; may be empty.
    pub effort: String,
    /// When it was written, unix ms.
    pub created_at: i64,
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Explanation> {
    Ok(Explanation {
        key: ExplanationKey {
            repo_id: row.get(0)?,
            merge_base: row.get(1)?,
            head_tip: row.get(2)?,
            scope: row.get(3)?,
        },
        body_md: row.get(4)?,
        model: row.get(5)?,
        effort: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// Async access to `explanations`.
#[derive(Clone, Debug)]
pub struct ExplanationStore {
    service: StorageService,
}

impl ExplanationStore {
    pub(crate) fn new(service: StorageService) -> Self {
        Self { service }
    }

    /// The explanation stored for `key`, if any.
    pub async fn get(&self, key: ExplanationKey) -> Result<Option<Explanation>> {
        self.service
            .call("ExplanationStore::get", move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT repo_id, merge_base, head_tip, scope, body_md, model, effort,
                            created_at
                     FROM explanations
                     WHERE repo_id = ?1 AND merge_base = ?2 AND head_tip = ?3 AND scope = ?4",
                )?;
                Ok(stmt
                    .query_row(
                        params![key.repo_id, key.merge_base, key.head_tip, key.scope],
                        from_row,
                    )
                    .optional()?)
            })
            .await
    }

    /// Stores an explanation, replacing the one of the same key, and drops
    /// the repository's rows beyond [`MAX_EXPLANATIONS_PER_REPO`].
    pub async fn put(
        &self,
        key: ExplanationKey,
        body_md: String,
        model: String,
        effort: String,
    ) -> Result<Explanation> {
        self.service
            .call("ExplanationStore::put", move |conn| {
                let explanation = Explanation {
                    key,
                    body_md,
                    model,
                    effort,
                    created_at: now_ms(),
                };
                let tx = conn.transaction()?;
                tx.execute(
                    "INSERT INTO explanations (repo_id, merge_base, head_tip, scope, body_md,
                                               model, effort, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(repo_id, merge_base, head_tip, scope) DO UPDATE SET
                         body_md = excluded.body_md,
                         model = excluded.model,
                         effort = excluded.effort,
                         created_at = excluded.created_at",
                    params![
                        explanation.key.repo_id,
                        explanation.key.merge_base,
                        explanation.key.head_tip,
                        explanation.key.scope,
                        explanation.body_md,
                        explanation.model,
                        explanation.effort,
                        explanation.created_at
                    ],
                )?;
                tx.execute(
                    "DELETE FROM explanations
                     WHERE repo_id = ?1 AND rowid NOT IN (
                         SELECT rowid FROM explanations WHERE repo_id = ?1
                         ORDER BY created_at DESC LIMIT ?2)",
                    params![explanation.key.repo_id, MAX_EXPLANATIONS_PER_REPO as i64],
                )?;
                tx.commit()?;
                Ok(explanation)
            })
            .await
    }
}
