//! Clip and clip-asset repository methods with strict owner isolation.

use std::str::FromStr;

use chrono::Utc;
use clipforge_api::{AudioTrackInfo, ClipDto, GameInfo, UpdateClipRequest};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::models::{
    ClipAssetKind, ClipAssetRecord, NewClip, NewClipAsset, parse_dt, parse_opt_dt,
    str_to_visibility, visibility_to_str,
};
use crate::{Database, DbError, Result};

pub(crate) fn map_clip_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClipDto> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let owner_id: String = row.get(1)?;
    let title: String = row.get(2)?;
    let game_json: Option<String> = row.get(3)?;
    let game: Option<GameInfo> = match game_json {
        Some(ref s) => Some(serde_json::from_str(s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    let duration_ms = u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0);
    let width = u32::try_from(row.get::<_, i64>(5)?).unwrap_or(0);
    let height = u32::try_from(row.get::<_, i64>(6)?).unwrap_or(0);
    let fps: f64 = row.get(7)?;
    let size_bytes = u64::try_from(row.get::<_, i64>(8)?).unwrap_or(0);
    let codec: String = row.get(9)?;
    let content_type: String = row.get(10)?;
    let audio_tracks_json: String = row.get(11)?;
    let audio_tracks: Vec<AudioTrackInfo> =
        serde_json::from_str(&audio_tracks_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let tags_json: String = row.get(12)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let notes: Option<String> = row.get(13)?;
    let vis_str: String = row.get(14)?;
    let visibility = str_to_visibility(&vis_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(14, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_str: String = row.get(15)?;
    let uploaded_str: String = row.get(16)?;
    let expires_str: Option<String> = row.get(17)?;
    let share_id: Option<String> = row.get(18)?;

    let created_at = parse_dt(&created_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(15, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let uploaded_at = parse_dt(&uploaded_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(16, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let expires_at = parse_opt_dt(expires_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(17, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(ClipDto {
        id,
        owner_id,
        title,
        game,
        duration_ms,
        width,
        height,
        fps: fps as f32,
        size_bytes,
        codec,
        content_type,
        audio_tracks,
        tags,
        notes,
        visibility,
        share_id,
        share_url: None,
        stream_url: None,
        thumbnail_url: None,
        created_at,
        uploaded_at,
        expires_at,
    })
}

pub(crate) fn map_asset_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClipAssetRecord> {
    let clip_id_str: String = row.get(1)?;
    let clip_id = Uuid::parse_str(&clip_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let kind_str: String = row.get(3)?;
    let asset_kind = ClipAssetKind::from_str(&kind_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let size_bytes = u64::try_from(row.get::<_, i64>(6)?).unwrap_or(0);
    let created_str: String = row.get(8)?;
    let created_at = parse_dt(&created_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(ClipAssetRecord {
        id: row.get(0)?,
        clip_id,
        owner_id: row.get(2)?,
        asset_kind,
        storage_key: row.get(4)?,
        content_type: row.get(5)?,
        size_bytes,
        sha256: row.get(7)?,
        created_at,
    })
}

pub(crate) fn fetch_clip_by_id_and_owner(
    conn: &Connection,
    owner_id: &str,
    clip_id: Uuid,
) -> Result<Option<ClipDto>> {
    conn.query_row(
        "SELECT c.id, c.owner_id, c.title, c.game_json, c.duration_ms, c.width, c.height,
                c.fps, c.size_bytes, c.codec, c.content_type, c.audio_tracks_json, c.tags_json,
                c.notes, c.visibility, c.created_at, c.uploaded_at, c.expires_at,
                (SELECT s.share_id FROM shares s
                 WHERE s.clip_id = c.id AND s.revoked_at IS NULL
                 ORDER BY s.created_at DESC LIMIT 1) AS share_id
         FROM clips c
         WHERE c.id = ?1 AND c.owner_id = ?2",
        params![clip_id.to_string(), owner_id],
        map_clip_row,
    )
    .optional()
    .map_err(DbError::from)
}

pub(crate) fn insert_clip_row(conn: &Connection, clip: &NewClip) -> Result<()> {
    let now_str = Utc::now().to_rfc3339();
    let game_json = clip.game.as_ref().map(serde_json::to_string).transpose()?;
    let audio_tracks_json = serde_json::to_string(&clip.audio_tracks)?;
    let tags_json = serde_json::to_string(&clip.tags)?;

    conn.execute(
        "INSERT INTO clips (
            id, owner_id, title, game_json, duration_ms, width, height, fps,
            size_bytes, codec, content_type, audio_tracks_json, tags_json, notes,
            visibility, created_at, uploaded_at, expires_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            clip.id.to_string(),
            clip.owner_id,
            clip.title,
            game_json,
            i64::try_from(clip.duration_ms).unwrap_or(i64::MAX),
            i64::from(clip.width),
            i64::from(clip.height),
            f64::from(clip.fps),
            i64::try_from(clip.size_bytes).unwrap_or(i64::MAX),
            clip.codec,
            clip.content_type,
            audio_tracks_json,
            tags_json,
            clip.notes,
            visibility_to_str(clip.visibility),
            clip.created_at.to_rfc3339(),
            clip.uploaded_at.to_rfc3339(),
            clip.expires_at.map(|dt| dt.to_rfc3339()),
            now_str,
        ],
    )?;
    Ok(())
}

pub(crate) fn insert_asset_row(conn: &Connection, asset: &NewClipAsset) -> Result<ClipAssetRecord> {
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let size_i64 = i64::try_from(asset.size_bytes)
        .map_err(|_| DbError::InvalidInput("asset size_bytes exceeds i64::MAX".into()))?;
    conn.execute(
        "INSERT INTO clip_assets (id, clip_id, owner_id, asset_kind, storage_key, content_type, size_bytes, sha256, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            asset.id,
            asset.clip_id.to_string(),
            asset.owner_id,
            asset.asset_kind.as_str(),
            asset.storage_key,
            asset.content_type,
            size_i64,
            asset.sha256,
            now_str,
        ],
    )?;
    Ok(ClipAssetRecord {
        id: asset.id.clone(),
        clip_id: asset.clip_id,
        owner_id: asset.owner_id.clone(),
        asset_kind: asset.asset_kind,
        storage_key: asset.storage_key.clone(),
        content_type: asset.content_type.clone(),
        size_bytes: asset.size_bytes,
        sha256: asset.sha256.clone(),
        created_at: now,
    })
}

impl Database {
    /// Atomically creates a clip and its assets, enforcing and updating storage quota.
    pub async fn create_clip_with_assets(
        &self,
        clip: NewClip,
        assets: Vec<NewClipAsset>,
    ) -> Result<ClipDto> {
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let added_bytes: u64 = if assets.is_empty() {
                clip.size_bytes
            } else {
                assets.iter().map(|a| a.size_bytes).sum()
            };

            let (used, reserved, limit): (u64, u64, u64) = tx
                .query_row(
                    "SELECT su.used_bytes, su.reserved_bytes, u.quota_limit_bytes
                     FROM users u
                     INNER JOIN storage_usage su ON su.user_id = u.id
                     WHERE u.id = ?1 AND u.is_active = 1",
                    params![clip.owner_id],
                    |row| {
                        Ok((
                            u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                            u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                            u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| {
                    DbError::NotFound(format!("active user '{}' not found", clip.owner_id))
                })?;

            if used.saturating_add(reserved).saturating_add(added_bytes) > limit {
                return Err(DbError::QuotaExceeded {
                    used_bytes: used,
                    reserved_bytes: reserved,
                    requested_bytes: added_bytes,
                    limit_bytes: limit,
                });
            }

            insert_clip_row(&tx, &clip)?;
            for asset in &assets {
                if asset.owner_id != clip.owner_id || asset.clip_id != clip.id {
                    return Err(DbError::InvalidInput(
                        "asset owner_id/clip_id must match clip".into(),
                    ));
                }
                insert_asset_row(&tx, asset)?;
            }

            let now_str = Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE storage_usage
                 SET used_bytes = used_bytes + ?1, clip_count = clip_count + 1, updated_at = ?2
                 WHERE user_id = ?3",
                params![
                    i64::try_from(added_bytes).unwrap_or(i64::MAX),
                    now_str,
                    clip.owner_id
                ],
            )?;

            let dto = fetch_clip_by_id_and_owner(&tx, &clip.owner_id, clip.id)?
                .ok_or_else(|| DbError::Internal("inserted clip missing".into()))?;
            tx.commit()?;
            Ok(dto)
        })
        .await
    }

    /// Fetches a clip strictly owned by `owner_id`.
    pub async fn get_clip_for_owner(
        &self,
        owner_id: &str,
        clip_id: Uuid,
    ) -> Result<Option<ClipDto>> {
        let owner_id = owner_id.to_string();
        self.with_conn(move |conn| fetch_clip_by_id_and_owner(conn, &owner_id, clip_id))
            .await
    }

    /// Lists clips strictly owned by `owner_id`, newest uploaded first.
    pub async fn list_clips_for_owner(
        &self,
        owner_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ClipDto>> {
        let owner_id = owner_id.to_string();
        let limit_i64 = i64::try_from(limit.min(500)).unwrap_or(500);
        let offset_i64 = i64::try_from(offset).unwrap_or(0);
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT c.id, c.owner_id, c.title, c.game_json, c.duration_ms, c.width, c.height,
                        c.fps, c.size_bytes, c.codec, c.content_type, c.audio_tracks_json, c.tags_json,
                        c.notes, c.visibility, c.created_at, c.uploaded_at, c.expires_at,
                        (SELECT s.share_id FROM shares s
                         WHERE s.clip_id = c.id AND s.revoked_at IS NULL
                         ORDER BY s.created_at DESC LIMIT 1) AS share_id
                 FROM clips c
                 WHERE c.owner_id = ?1
                 ORDER BY c.uploaded_at DESC
                 LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt
                .query_map(params![owner_id, limit_i64, offset_i64], map_clip_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Updates mutable metadata on a clip strictly owned by `owner_id`.
    pub async fn update_clip_for_owner(
        &self,
        owner_id: &str,
        clip_id: Uuid,
        update: UpdateClipRequest,
    ) -> Result<Option<ClipDto>> {
        let owner_id = owner_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(existing) = fetch_clip_by_id_and_owner(&tx, &owner_id, clip_id)? else {
                return Ok(None);
            };

            let title = update.title.unwrap_or(existing.title);
            let notes = if update.notes.is_some() {
                update.notes
            } else {
                existing.notes
            };
            let tags = update.tags.unwrap_or(existing.tags);
            let visibility = update.visibility.unwrap_or(existing.visibility);
            let tags_json = serde_json::to_string(&tags)?;
            let now_str = Utc::now().to_rfc3339();

            tx.execute(
                "UPDATE clips
                 SET title = ?1, notes = ?2, tags_json = ?3, visibility = ?4, updated_at = ?5
                 WHERE id = ?6 AND owner_id = ?7",
                params![
                    title,
                    notes,
                    tags_json,
                    visibility_to_str(visibility),
                    now_str,
                    clip_id.to_string(),
                    owner_id,
                ],
            )?;

            let updated = fetch_clip_by_id_and_owner(&tx, &owner_id, clip_id)?;
            tx.commit()?;
            Ok(updated)
        })
        .await
    }

    /// Deletes a clip strictly owned by `owner_id`, releasing its `storage_usage`
    /// and returning its `clip_assets` so the caller can purge object storage.
    pub async fn delete_clip_for_owner(
        &self,
        owner_id: &str,
        clip_id: Uuid,
    ) -> Result<Option<Vec<ClipAssetRecord>>> {
        let owner_id = owner_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let clip_size: Option<i64> = tx
                .query_row(
                    "SELECT size_bytes FROM clips WHERE id = ?1 AND owner_id = ?2",
                    params![clip_id.to_string(), owner_id],
                    |row| row.get(0),
                )
                .optional()?;

            let Some(clip_size_i64) = clip_size else {
                return Ok(None);
            };

            let assets = {
                let mut stmt = tx.prepare(
                    "SELECT id, clip_id, owner_id, asset_kind, storage_key, content_type, size_bytes, sha256, created_at
                     FROM clip_assets WHERE clip_id = ?1 AND owner_id = ?2",
                )?;
                stmt.query_map(params![clip_id.to_string(), owner_id], map_asset_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };

            let removed_bytes: u64 = if assets.is_empty() {
                u64::try_from(clip_size_i64).unwrap_or(0)
            } else {
                assets.iter().map(|a| a.size_bytes).sum()
            };

            tx.execute(
                "DELETE FROM clips WHERE id = ?1 AND owner_id = ?2",
                params![clip_id.to_string(), owner_id],
            )?;

            let now_str = Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE storage_usage
                 SET used_bytes = MAX(0, used_bytes - ?1),
                     clip_count = MAX(0, clip_count - 1),
                     updated_at = ?2
                 WHERE user_id = ?3",
                params![
                    i64::try_from(removed_bytes).unwrap_or(i64::MAX),
                    now_str,
                    owner_id
                ],
            )?;

            tx.commit()?;
            Ok(Some(assets))
        })
        .await
    }

    /// Inserts or replaces a clip asset owned by `owner_id` (e.g. thumbnail or preview),
    /// enforcing quota on any size increase.
    pub async fn upsert_clip_asset_for_owner(
        &self,
        owner_id: &str,
        asset: NewClipAsset,
    ) -> Result<ClipAssetRecord> {
        if asset.owner_id != owner_id {
            return Err(DbError::InvalidInput(
                "asset.owner_id must match owner_id".into(),
            ));
        }
        let owner_id = owner_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let owns_clip: bool = tx
                .query_row(
                    "SELECT 1 FROM clips WHERE id = ?1 AND owner_id = ?2",
                    params![asset.clip_id.to_string(), owner_id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);

            if !owns_clip {
                return Err(DbError::NotFound(format!(
                    "clip '{}' not found",
                    asset.clip_id
                )));
            }

            let old_size: u64 = tx
                .query_row(
                    "SELECT size_bytes FROM clip_assets WHERE clip_id = ?1 AND asset_kind = ?2",
                    params![asset.clip_id.to_string(), asset.asset_kind.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(0);

            if asset.size_bytes > old_size {
                let delta = asset.size_bytes - old_size;
                let (used, reserved, limit): (u64, u64, u64) = tx.query_row(
                    "SELECT su.used_bytes, su.reserved_bytes, u.quota_limit_bytes
                     FROM users u
                     INNER JOIN storage_usage su ON su.user_id = u.id
                     WHERE u.id = ?1",
                    params![owner_id],
                    |row| {
                        Ok((
                            u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                            u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                            u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                        ))
                    },
                )?;
                if used.saturating_add(reserved).saturating_add(delta) > limit {
                    return Err(DbError::QuotaExceeded {
                        used_bytes: used,
                        reserved_bytes: reserved,
                        requested_bytes: delta,
                        limit_bytes: limit,
                    });
                }
            }

            tx.execute(
                "DELETE FROM clip_assets WHERE clip_id = ?1 AND asset_kind = ?2",
                params![asset.clip_id.to_string(), asset.asset_kind.as_str()],
            )?;
            let record = insert_asset_row(&tx, &asset)?;

            let now_str = Utc::now().to_rfc3339();
            let delta_i64 = i64::try_from(asset.size_bytes).unwrap_or(i64::MAX)
                - i64::try_from(old_size).unwrap_or(i64::MAX);
            tx.execute(
                "UPDATE storage_usage SET used_bytes = MAX(0, used_bytes + ?1), updated_at = ?2 WHERE user_id = ?3",
                params![delta_i64, now_str, owner_id],
            )?;

            tx.commit()?;
            Ok(record)
        })
        .await
    }

    /// Fetches a specific asset kind for a clip strictly owned by `owner_id`.
    pub async fn get_clip_asset_for_owner(
        &self,
        owner_id: &str,
        clip_id: Uuid,
        kind: ClipAssetKind,
    ) -> Result<Option<ClipAssetRecord>> {
        let owner_id = owner_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT id, clip_id, owner_id, asset_kind, storage_key, content_type, size_bytes, sha256, created_at
                 FROM clip_assets
                 WHERE clip_id = ?1 AND owner_id = ?2 AND asset_kind = ?3",
                params![clip_id.to_string(), owner_id, kind.as_str()],
                map_asset_row,
            )
            .optional()
            .map_err(DbError::from)
        })
        .await
    }

    /// Lists all assets for a clip strictly owned by `owner_id`.
    pub async fn list_clip_assets_for_owner(
        &self,
        owner_id: &str,
        clip_id: Uuid,
    ) -> Result<Vec<ClipAssetRecord>> {
        let owner_id = owner_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, clip_id, owner_id, asset_kind, storage_key, content_type, size_bytes, sha256, created_at
                 FROM clip_assets
                 WHERE clip_id = ?1 AND owner_id = ?2",
            )?;
            let rows = stmt
                .query_map(params![clip_id.to_string(), owner_id], map_asset_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }
}
