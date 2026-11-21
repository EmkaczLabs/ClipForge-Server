//! Share link repository methods (create, list, revoke, public lookup).

use chrono::{DateTime, Utc};
use clipforge_api::ShareVisibility;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::models::{
    ClipAssetKind, PublicShareLookup, ShareLookupOutcome, ShareRecord, parse_dt, parse_opt_dt,
    str_to_visibility, visibility_to_str,
};
use crate::repos::clips::{fetch_clip_by_id_and_owner, map_asset_row};
use crate::{Database, DbError, Result};

fn map_share_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ShareRecord> {
    let clip_id_str: String = row.get(1)?;
    let clip_id = Uuid::parse_str(&clip_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let vis_str: String = row.get(3)?;
    let visibility = str_to_visibility(&vis_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at = parse_dt(&row.get::<_, String>(4)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let expires_at = parse_opt_dt(row.get(5)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let revoked_at = parse_opt_dt(row.get(6)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(ShareRecord {
        share_id: row.get(0)?,
        clip_id,
        owner_id: row.get(2)?,
        visibility,
        created_at,
        expires_at,
        revoked_at,
    })
}

impl Database {
    /// Creates a share link for a clip strictly owned by `owner_id`.
    pub async fn create_share(
        &self,
        owner_id: &str,
        clip_id: Uuid,
        share_id: String,
        visibility: ShareVisibility,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<ShareRecord> {
        let owner_id = owner_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let owns_clip: bool = tx
                .query_row(
                    "SELECT 1 FROM clips WHERE id = ?1 AND owner_id = ?2",
                    params![clip_id.to_string(), owner_id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !owns_clip {
                return Err(DbError::NotFound(format!("clip '{clip_id}' not found")));
            }

            let now = Utc::now();
            let now_str = now.to_rfc3339();
            let expires_str = expires_at.map(|dt| dt.to_rfc3339());
            tx.execute(
                "INSERT INTO shares (share_id, clip_id, owner_id, visibility, created_at, expires_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![
                    share_id,
                    clip_id.to_string(),
                    owner_id,
                    visibility_to_str(visibility),
                    now_str,
                    expires_str,
                ],
            )?;

            tx.commit()?;
            Ok(ShareRecord {
                share_id,
                clip_id,
                owner_id,
                visibility,
                created_at: now,
                expires_at,
                revoked_at: None,
            })
        })
        .await
    }

    /// Lists share links for a clip strictly owned by `owner_id`.
    pub async fn list_shares_for_clip(
        &self,
        owner_id: &str,
        clip_id: Uuid,
    ) -> Result<Vec<ShareRecord>> {
        let owner_id = owner_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT share_id, clip_id, owner_id, visibility, created_at, expires_at, revoked_at
                 FROM shares
                 WHERE clip_id = ?1 AND owner_id = ?2
                 ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map(params![clip_id.to_string(), owner_id], map_share_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Updates visibility on an active share link and syncs the owning clip and its active shares.
    pub async fn update_share_visibility(
        &self,
        owner_id: &str,
        share_id: &str,
        visibility: ShareVisibility,
    ) -> Result<Option<ShareRecord>> {
        let owner_id = owner_id.to_string();
        let share_id = share_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<ShareRecord> = tx
                .query_row(
                    "SELECT share_id, clip_id, owner_id, visibility, created_at, expires_at, revoked_at
                     FROM shares
                     WHERE share_id = ?1 AND owner_id = ?2 AND revoked_at IS NULL",
                    params![share_id, owner_id],
                    map_share_row,
                )
                .optional()?;

            let Some(mut share) = existing else {
                return Ok(None);
            };

            let vis_str = visibility_to_str(visibility);
            let now_str = Utc::now().to_rfc3339();

            tx.execute(
                "UPDATE shares
                 SET visibility = ?1
                 WHERE clip_id = ?2 AND owner_id = ?3 AND revoked_at IS NULL",
                params![vis_str, share.clip_id.to_string(), owner_id],
            )?;
            tx.execute(
                "UPDATE clips
                 SET visibility = ?1, updated_at = ?2
                 WHERE id = ?3 AND owner_id = ?4",
                params![vis_str, now_str, share.clip_id.to_string(), owner_id],
            )?;

            tx.commit()?;
            share.visibility = visibility;
            Ok(Some(share))
        })
        .await
    }

    /// Revokes a share link strictly owned by `owner_id`.
    pub async fn revoke_share(&self, owner_id: &str, share_id: &str) -> Result<bool> {
        let owner_id = owner_id.to_string();
        let share_id = share_id.to_string();
        self.with_conn(move |conn| {
            let now_str = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE shares SET revoked_at = ?1
                 WHERE share_id = ?2 AND owner_id = ?3 AND revoked_at IS NULL",
                params![now_str, share_id, owner_id],
            )?;
            Ok(updated > 0)
        })
        .await
    }

    /// Resolves a public share token, distinguishing `NotFound`, `Revoked`, `Expired`,
    /// `Private`, and `Active`.
    pub async fn lookup_public_share(
        &self,
        share_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ShareLookupOutcome> {
        let share_id = share_id.to_string();
        self.with_conn(move |conn| {
            let share: Option<ShareRecord> = conn
                .query_row(
                    "SELECT share_id, clip_id, owner_id, visibility, created_at, expires_at, revoked_at
                     FROM shares WHERE share_id = ?1",
                    params![share_id],
                    map_share_row,
                )
                .optional()?;

            let Some(share) = share else {
                return Ok(ShareLookupOutcome::NotFound);
            };
            if share.revoked_at.is_some() {
                return Ok(ShareLookupOutcome::Revoked);
            }
            if let Some(exp) = share.expires_at
                && exp <= now
            {
                return Ok(ShareLookupOutcome::Expired);
            }
            if share.visibility == ShareVisibility::Private {
                return Ok(ShareLookupOutcome::Private);
            }

            let Some(clip) = fetch_clip_by_id_and_owner(conn, &share.owner_id, share.clip_id)?
            else {
                return Ok(ShareLookupOutcome::NotFound);
            };
            if let Some(exp) = clip.expires_at
                && exp <= now
            {
                return Ok(ShareLookupOutcome::Expired);
            }

            let mut stmt = conn.prepare(
                "SELECT id, clip_id, owner_id, asset_kind, storage_key, content_type, size_bytes, sha256, created_at
                 FROM clip_assets WHERE clip_id = ?1 AND owner_id = ?2",
            )?;
            let assets = stmt
                .query_map(params![share.clip_id.to_string(), share.owner_id], map_asset_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let original_asset = assets
                .iter()
                .find(|a| a.asset_kind == ClipAssetKind::Original)
                .cloned();
            let thumbnail_asset = assets
                .iter()
                .find(|a| a.asset_kind == ClipAssetKind::Thumbnail)
                .cloned();

            Ok(ShareLookupOutcome::Active(Box::new(PublicShareLookup {
                share,
                clip,
                original_asset,
                thumbnail_asset,
            })))
        })
        .await
    }
}
