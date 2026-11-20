//! User repository methods.

use chrono::Utc;
use clipforge_api::UserRole;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::models::{NewUser, UserRecord, parse_dt, role_to_str, str_to_role};
use crate::{Database, DbError, Result};

fn map_user_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserRecord> {
    let role_str: String = row.get(4)?;
    let is_active_int: i64 = row.get(5)?;
    let quota_limit_i64: i64 = row.get(6)?;
    let created_str: String = row.get(7)?;
    let updated_str: String = row.get(8)?;

    let role = str_to_role(&role_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at = parse_dt(&created_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let updated_at = parse_dt(&updated_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(UserRecord {
        id: row.get(0)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        password_hash: row.get(3)?,
        role,
        is_active: is_active_int != 0,
        quota_limit_bytes: u64::try_from(quota_limit_i64).unwrap_or(0),
        created_at,
        updated_at,
    })
}

pub(crate) fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: 2067 | 1555,
            },
            _
        )
    )
}

pub(crate) fn insert_user(conn: &mut Connection, new_user: NewUser) -> Result<UserRecord> {
    let username = new_user.username.trim().to_string();
    if username.is_empty() {
        return Err(DbError::InvalidInput("username cannot be empty".into()));
    }
    let quota_i64 = i64::try_from(new_user.quota_limit_bytes)
        .map_err(|_| DbError::InvalidInput("quota_limit_bytes exceeds i64::MAX".into()))?;
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let role_str = role_to_str(new_user.role);

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let insert_res = tx.execute(
        "INSERT INTO users (id, username, display_name, password_hash, role, is_active, quota_limit_bytes, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8)",
        params![
            new_user.id,
            username,
            new_user.display_name,
            new_user.password_hash,
            role_str,
            quota_i64,
            now_str,
            now_str,
        ],
    );

    if let Err(err) = insert_res {
        if is_unique_violation(&err) {
            return Err(DbError::Conflict(format!(
                "username '{username}' already exists"
            )));
        }
        return Err(DbError::Sqlite(err));
    }

    tx.execute(
        "INSERT INTO storage_usage (user_id, used_bytes, reserved_bytes, clip_count, updated_at)
         VALUES (?1, 0, 0, 0, ?2)",
        params![new_user.id, now_str],
    )?;

    tx.commit()?;

    Ok(UserRecord {
        id: new_user.id,
        username,
        display_name: new_user.display_name,
        password_hash: new_user.password_hash,
        role: new_user.role,
        is_active: true,
        quota_limit_bytes: new_user.quota_limit_bytes,
        created_at: now,
        updated_at: now,
    })
}

impl Database {
    /// Creates a new user and initializes their `storage_usage` row atomically.
    pub async fn create_user(&self, new_user: NewUser) -> Result<UserRecord> {
        self.with_conn(move |conn| insert_user(conn, new_user))
            .await
    }

    /// Fetches a user by their unique ID.
    pub async fn get_user_by_id(&self, user_id: &str) -> Result<Option<UserRecord>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT id, username, display_name, password_hash, role, is_active, quota_limit_bytes, created_at, updated_at
                 FROM users WHERE id = ?1",
                params![user_id],
                map_user_row,
            )
            .optional()
            .map_err(DbError::from)
        })
        .await
    }

    /// Fetches a user by username (case-insensitive via `COLLATE NOCASE`).
    pub async fn get_user_by_username(&self, username: &str) -> Result<Option<UserRecord>> {
        let username = username.trim().to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT id, username, display_name, password_hash, role, is_active, quota_limit_bytes, created_at, updated_at
                 FROM users WHERE username = ?1",
                params![username],
                map_user_row,
            )
            .optional()
            .map_err(DbError::from)
        })
        .await
    }

    /// Lists users ordered by creation timestamp.
    pub async fn list_users(&self, limit: usize, offset: usize) -> Result<Vec<UserRecord>> {
        let limit_i64 = i64::try_from(limit.min(500)).unwrap_or(500);
        let offset_i64 = i64::try_from(offset).unwrap_or(0);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, username, display_name, password_hash, role, is_active, quota_limit_bytes, created_at, updated_at
                 FROM users ORDER BY created_at ASC LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt
                .query_map(params![limit_i64, offset_i64], map_user_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Returns total number of users.
    pub async fn count_users(&self) -> Result<u64> {
        self.with_conn(|conn| {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
        .await
    }

    /// Returns total number of active admin users.
    pub async fn count_admins(&self) -> Result<u64> {
        self.with_conn(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM users WHERE role = 'admin' AND is_active = 1",
                [],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
        .await
    }

    /// Updates a user's password hash.
    pub async fn update_user_password(&self, user_id: &str, password_hash: &str) -> Result<()> {
        let user_id = user_id.to_string();
        let password_hash = password_hash.to_string();
        self.with_conn(move |conn| {
            let now_str = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3",
                params![password_hash, now_str, user_id],
            )?;
            if updated == 0 {
                return Err(DbError::NotFound(format!("user '{user_id}' not found")));
            }
            Ok(())
        })
        .await
    }

    /// Updates a user's role and active flag.
    pub async fn update_user_role_and_status(
        &self,
        user_id: &str,
        role: UserRole,
        is_active: bool,
    ) -> Result<()> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            let now_str = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE users SET role = ?1, is_active = ?2, updated_at = ?3 WHERE id = ?4",
                params![role_to_str(role), i64::from(is_active), now_str, user_id],
            )?;
            if updated == 0 {
                return Err(DbError::NotFound(format!("user '{user_id}' not found")));
            }
            Ok(())
        })
        .await
    }

    /// Updates a user's storage quota limit in bytes.
    pub async fn update_user_quota_limit(
        &self,
        user_id: &str,
        quota_limit_bytes: u64,
    ) -> Result<()> {
        let user_id = user_id.to_string();
        let quota_i64 = i64::try_from(quota_limit_bytes)
            .map_err(|_| DbError::InvalidInput("quota_limit_bytes exceeds i64::MAX".into()))?;
        self.with_conn(move |conn| {
            let now_str = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE users SET quota_limit_bytes = ?1, updated_at = ?2 WHERE id = ?3",
                params![quota_i64, now_str, user_id],
            )?;
            if updated == 0 {
                return Err(DbError::NotFound(format!("user '{user_id}' not found")));
            }
            Ok(())
        })
        .await
    }

    /// Deletes a user (cascading tokens, clips, assets, uploads, shares, storage_usage).
    pub async fn delete_user(&self, user_id: &str) -> Result<bool> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            let deleted = conn.execute("DELETE FROM users WHERE id = ?1", params![user_id])?;
            Ok(deleted > 0)
        })
        .await
    }
}
