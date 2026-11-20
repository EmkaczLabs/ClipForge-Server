//! Server settings key-value repository methods.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};

use crate::{Database, DbError, Result};

impl Database {
    /// Reads a setting value by key.
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT value FROM server_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(DbError::from)
        })
        .await
    }

    /// Inserts or updates a setting value.
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let key = key.to_string();
        let value = value.to_string();
        self.with_conn(move |conn| {
            let now_str = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO server_settings (key, value, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![key, value, now_str],
            )?;
            Ok(())
        })
        .await
    }

    /// Deletes a setting by key.
    pub async fn delete_setting(&self, key: &str) -> Result<bool> {
        let key = key.to_string();
        self.with_conn(move |conn| {
            let deleted =
                conn.execute("DELETE FROM server_settings WHERE key = ?1", params![key])?;
            Ok(deleted > 0)
        })
        .await
    }

    /// Lists all server settings ordered by key.
    pub async fn list_settings(&self) -> Result<Vec<(String, String)>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT key, value FROM server_settings ORDER BY key ASC")?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }
}
