//! SQLite persistence layer for ClipForge Server.
//!
//! Owned by `@srv-db-auth`. Provides user, API token, clip, clip asset,
//! upload session, share, storage usage, and settings persistence using `rusqlite`.

pub mod models;
pub mod repos;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

pub use models::*;

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
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error(
        "storage quota exceeded (used={used_bytes}, reserved={reserved_bytes}, requested={requested_bytes}, limit={limit_bytes})"
    )]
    QuotaExceeded {
        used_bytes: u64,
        reserved_bytes: u64,
        requested_bytes: u64,
        limit_bytes: u64,
    },
    #[error("migration error: {0}")]
    Migration(String),
    #[error("internal database error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, DbError>;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "0001_initial",
    sql: include_str!("../../../migrations/0001_initial.sql"),
}];

const EXPECTED_TABLES: &[&str] = &[
    "_migrations",
    "users",
    "storage_usage",
    "api_tokens",
    "clips",
    "clip_assets",
    "uploads",
    "shares",
    "server_settings",
];

fn fnv1a_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn configure_connection(conn: &Connection, is_memory: bool) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
    if !is_memory {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    }
    let fk_enabled: i64 = conn.query_row("PRAGMA foreign_keys;", [], |row| row.get(0))?;
    if fk_enabled != 1 {
        return Err(DbError::Migration(
            "failed to enable PRAGMA foreign_keys = ON".into(),
        ));
    }
    Ok(())
}

fn run_startup_migrations(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY NOT NULL,
            name TEXT NOT NULL,
            checksum TEXT NOT NULL,
            applied_at TEXT NOT NULL
        );",
    )?;

    for migration in MIGRATIONS {
        let checksum = fnv1a_hex(migration.sql);
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT name, checksum FROM _migrations WHERE version = ?1",
                params![migration.version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((existing_name, existing_checksum)) = existing {
            if existing_name != migration.name || existing_checksum != checksum {
                return Err(DbError::Migration(format!(
                    "migration v{} ({}) checksum mismatch",
                    migration.version, migration.name
                )));
            }
            continue;
        }

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO _migrations (version, name, checksum, applied_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                migration.version,
                migration.name,
                checksum,
                Utc::now().to_rfc3339()
            ],
        )?;
        tx.commit()?;
    }

    for table in EXPECTED_TABLES {
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(DbError::Migration(format!(
                "startup check failed: expected table '{table}' is missing"
            )));
        }
    }

    Ok(())
}

/// Handle to the ClipForge Server SQLite database.
#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// Opens (or creates) the SQLite database at `path` and runs startup migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let path_for_open = path_buf.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let is_memory = path_for_open == Path::new(":memory:");
            if !is_memory
                && let Some(parent) = path_for_open.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|e| {
                    DbError::Internal(format!(
                        "failed to create database directory '{}': {e}",
                        parent.display()
                    ))
                })?;
            }
            let mut conn = if is_memory {
                Connection::open_in_memory()?
            } else {
                Connection::open(&path_for_open)?
            };
            configure_connection(&conn, is_memory)?;
            run_startup_migrations(&mut conn)?;
            Ok(conn)
        })
        .await
        .map_err(|e| DbError::Internal(format!("blocking db open task failed: {e}")))??;

        Ok(Self {
            path: path_buf,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Opens an isolated in-memory SQLite database with migrations applied (for tests).
    pub async fn open_in_memory() -> Result<Self> {
        Self::open(":memory:").await
    }

    /// Returns the configured database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Performs a lightweight health check query against the database.
    pub async fn health_check(&self) -> Result<()> {
        self.with_conn(|conn| {
            let one: i64 = conn.query_row("SELECT 1", [], |row| row.get(0))?;
            let fk: i64 = conn.query_row("PRAGMA foreign_keys;", [], |row| row.get(0))?;
            if one != 1 || fk != 1 {
                return Err(DbError::Internal(
                    "database health check invariant failed".into(),
                ));
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| DbError::Internal("database mutex poisoned".into()))?;
            f(&mut guard)
        })
        .await
        .map_err(|e| DbError::Internal(format!("blocking db task failed: {e}")))?
    }
}
