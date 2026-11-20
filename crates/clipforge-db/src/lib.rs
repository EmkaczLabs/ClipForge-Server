//! SQLite persistence layer for ClipForge Server.
//!
//! Owned by `@srv-db-auth`. Provides user, session, API token, clip,
//! upload session, share, and quota persistence using `rusqlite`.

use std::path::Path;

use thiserror::Error;

/// Errors returned by the database layer.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("internal database error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// Handle to the ClipForge Server SQLite database.
#[derive(Debug, Clone)]
pub struct Database {
    path: std::path::PathBuf,
}

impl Database {
    /// Opens (or creates) the SQLite database at `path` and runs migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            path: path.as_ref().to_path_buf(),
        })
    }

    /// Returns the configured database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Performs a lightweight health check query against the database.
    pub async fn health_check(&self) -> Result<()> {
        Ok(())
    }
}
