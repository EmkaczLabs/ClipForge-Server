//! API token repository methods (stores SHA-256 hashes only).

use chrono::{DateTime, Utc};
use clipforge_api::ApiTokenInfo;
use rusqlite::{OptionalExtension, params};

use crate::models::{ApiTokenWithUser, NewApiToken, parse_dt, parse_opt_dt, str_to_role};
use crate::{Database, DbError, Result};

fn map_token_info(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiTokenInfo> {
    let scopes_json: String = row.get(3)?;
    let created_str: String = row.get(4)?;
    let expires_str: Option<String> = row.get(5)?;
    let last_used_str: Option<String> = row.get(6)?;

    let scopes: Vec<String> = serde_json::from_str(&scopes_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at = parse_dt(&created_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let expires_at = parse_opt_dt(expires_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let last_used_at = parse_opt_dt(last_used_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(ApiTokenInfo {
        id: row.get(0)?,
        name: row.get(1)?,
        prefix: row.get(2)?,
        scopes,
        created_at,
        expires_at,
        last_used_at,
    })
}

impl Database {
    /// Persists a newly generated hashed API token.
    pub async fn create_api_token(&self, token: NewApiToken) -> Result<ApiTokenInfo> {
        self.with_conn(move |conn| {
            let now = Utc::now();
            let now_str = now.to_rfc3339();
            let scopes_json = serde_json::to_string(&token.scopes)?;
            let expires_str = token.expires_at.map(|dt| dt.to_rfc3339());

            conn.execute(
                "INSERT INTO api_tokens (id, user_id, name, prefix, token_hash, scopes_json, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    token.id,
                    token.user_id,
                    token.name,
                    token.prefix,
                    token.token_hash,
                    scopes_json,
                    now_str,
                    expires_str,
                ],
            )?;

            Ok(ApiTokenInfo {
                id: token.id,
                name: token.name,
                prefix: token.prefix,
                scopes: token.scopes,
                created_at: now,
                expires_at: token.expires_at,
                last_used_at: None,
            })
        })
        .await
    }

    /// Looks up a token by its SHA-256 hex digest along with the owning user.
    pub async fn find_api_token_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<ApiTokenWithUser>> {
        let token_hash = token_hash.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT t.id, t.name, t.prefix, t.scopes_json, t.created_at, t.expires_at, t.last_used_at,
                        t.revoked_at, u.id, u.username, u.role, u.is_active
                 FROM api_tokens t
                 INNER JOIN users u ON u.id = t.user_id
                 WHERE t.token_hash = ?1",
                params![token_hash],
                |row| {
                    let info = map_token_info(row)?;
                    let revoked_str: Option<String> = row.get(7)?;
                    let revoked_at = parse_opt_dt(revoked_str).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            7,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    let role_str: String = row.get(10)?;
                    let role = str_to_role(&role_str).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            10,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    let is_active_int: i64 = row.get(11)?;

                    Ok(ApiTokenWithUser {
                        token: info,
                        user_id: row.get(8)?,
                        username: row.get(9)?,
                        role,
                        is_active: is_active_int != 0,
                        revoked_at,
                    })
                },
            )
            .optional()
            .map_err(DbError::from)
        })
        .await
    }

    /// Updates `last_used_at` for a token.
    pub async fn touch_api_token_last_used(&self, token_id: &str, at: DateTime<Utc>) -> Result<()> {
        let token_id = token_id.to_string();
        let at_str = at.to_rfc3339();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE api_tokens SET last_used_at = ?1 WHERE id = ?2",
                params![at_str, token_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Lists non-revoked API tokens belonging strictly to `user_id`.
    pub async fn list_api_tokens_for_user(&self, user_id: &str) -> Result<Vec<ApiTokenInfo>> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, prefix, scopes_json, created_at, expires_at, last_used_at
                 FROM api_tokens
                 WHERE user_id = ?1 AND revoked_at IS NULL
                 ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map(params![user_id], map_token_info)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Revokes a single token owned by `user_id`. Returns `false` if not found,
    /// already revoked, or owned by another user.
    pub async fn revoke_api_token(&self, user_id: &str, token_id: &str) -> Result<bool> {
        let user_id = user_id.to_string();
        let token_id = token_id.to_string();
        self.with_conn(move |conn| {
            let now_str = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE api_tokens SET revoked_at = ?1
                 WHERE id = ?2 AND user_id = ?3 AND revoked_at IS NULL",
                params![now_str, token_id, user_id],
            )?;
            Ok(updated > 0)
        })
        .await
    }

    /// Revokes all active tokens for `user_id`, returning the count revoked.
    pub async fn revoke_all_api_tokens_for_user(&self, user_id: &str) -> Result<u64> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            let now_str = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE api_tokens SET revoked_at = ?1
                 WHERE user_id = ?2 AND revoked_at IS NULL",
                params![now_str, user_id],
            )?;
            Ok(updated as u64)
        })
        .await
    }
}
