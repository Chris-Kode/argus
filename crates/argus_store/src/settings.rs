//! The `settings` table: JSON values keyed by name.

use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::StorageService;
use crate::error::Result;

/// Async access to `settings`.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    service: StorageService,
}

impl SettingsStore {
    pub(crate) fn new(service: StorageService) -> Self {
        Self { service }
    }

    /// Reads and decodes a setting; `None` if it was never set.
    pub async fn get<T>(&self, key: impl Into<String>) -> Result<Option<T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        let key = key.into();
        self.service
            .call("SettingsStore::get", move |conn| {
                let raw: Option<String> = conn
                    .query_row(
                        "SELECT value_json FROM settings WHERE key = ?1",
                        [&key],
                        |row| row.get(0),
                    )
                    .optional()?;
                raw.map(|text| serde_json::from_str(&text))
                    .transpose()
                    .map_err(Into::into)
            })
            .await
    }

    /// Encodes and stores a setting, replacing any previous value.
    pub async fn set<T>(&self, key: impl Into<String>, value: &T) -> Result<()>
    where
        T: Serialize + ?Sized,
    {
        let key = key.into();
        let json = serde_json::to_string(value)?;
        self.service
            .call("SettingsStore::set", move |conn| {
                conn.execute(
                    "INSERT INTO settings (key, value_json) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
                    params![key, json],
                )?;
                Ok(())
            })
            .await
    }

    /// Removes a setting. Removing a missing key is not an error.
    pub async fn remove(&self, key: impl Into<String>) -> Result<()> {
        let key = key.into();
        self.service
            .call("SettingsStore::remove", move |conn| {
                conn.execute("DELETE FROM settings WHERE key = ?1", [&key])?;
                Ok(())
            })
            .await
    }
}
