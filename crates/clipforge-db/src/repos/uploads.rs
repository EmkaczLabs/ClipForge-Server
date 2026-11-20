//! Upload session repository methods with atomic quota reservation and cleanup.

use chrono::{DateTime, Utc};
use clipforge_api::{ClipDto, QuotaDto, UploadSessionStatus};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::models::{
    ClipAssetKind, NewClip, NewClipAsset, NewUploadSession, UploadMetadata, UploadSessionRecord,
    parse_dt, str_to_upload_status, str_to_visibility, upload_status_to_str, visibility_to_str,
};
use crate::repos::clips::{fetch_clip_by_id_and_owner, insert_asset_row, insert_clip_row};
use crate::{Database, DbError, Result};

fn map_upload_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UploadSessionRecord> {
    let clip_id_str: String = row.get(2)?;
    let clip_id = Uuid::parse_str(&clip_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status_str: String = row.get(3)?;
    let status = str_to_upload_status(&status_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let vis_str: String = row.get(8)?;
    let visibility = str_to_visibility(&vis_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let metadata_json: String = row.get(9)?;
    let metadata: UploadMetadata = serde_json::from_str(&metadata_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let uploaded_parts_json: String = row.get(17)?;
    let uploaded_parts: Vec<u32> = serde_json::from_str(&uploaded_parts_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(17, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at = parse_dt(&row.get::<_, String>(18)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(18, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let updated_at = parse_dt(&row.get::<_, String>(19)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(19, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let expires_at = parse_dt(&row.get::<_, String>(20)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(20, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(UploadSessionRecord {
        session_id: row.get(0)?,
        user_id: row.get(1)?,
        clip_id,
        status,
        title: row.get(4)?,
        filename: row.get(5)?,
        content_type: row.get(6)?,
        sha256: row.get(7)?,
        visibility,
        metadata,
        storage_key: row.get(10)?,
        multipart_upload_id: row.get(11)?,
        total_bytes: u64::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
        bytes_received: u64::try_from(row.get::<_, i64>(13)?).unwrap_or(0),
        reserved_bytes: u64::try_from(row.get::<_, i64>(14)?).unwrap_or(0),
        part_size_bytes: u64::try_from(row.get::<_, i64>(15)?).unwrap_or(1),
        total_parts: u32::try_from(row.get::<_, i64>(16)?).unwrap_or(0),
        uploaded_parts,
        created_at,
        updated_at,
        expires_at,
    })
}

const UPLOAD_SELECT_COLS: &str = "id, user_id, clip_id, status, title, filename, content_type, sha256,
    visibility, metadata_json, storage_key, multipart_upload_id, total_bytes, bytes_received,
    reserved_bytes, part_size_bytes, total_parts, uploaded_parts_json, created_at, updated_at, expires_at";

fn fetch_upload_for_owner(
    conn: &Connection,
    user_id: &str,
    session_id: &str,
) -> Result<Option<UploadSessionRecord>> {
    let sql = format!("SELECT {UPLOAD_SELECT_COLS} FROM uploads WHERE id = ?1 AND user_id = ?2");
    conn.query_row(&sql, params![session_id, user_id], map_upload_row)
        .optional()
        .map_err(DbError::from)
}

fn release_upload_reservation(
    conn: &Connection,
    user_id: &str,
    session_id: &str,
    target_status: UploadSessionStatus,
) -> Result<Option<UploadSessionRecord>> {
    let Some(existing) = fetch_upload_for_owner(conn, user_id, session_id)? else {
        return Ok(None);
    };

    if matches!(
        existing.status,
        UploadSessionStatus::Completed | UploadSessionStatus::Failed | UploadSessionStatus::Aborted
    ) {
        return Ok(Some(existing));
    }

    let now_str = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE uploads SET status = ?1, reserved_bytes = 0, updated_at = ?2
         WHERE id = ?3 AND user_id = ?4",
        params![
            upload_status_to_str(target_status),
            now_str,
            session_id,
            user_id
        ],
    )?;

    if existing.reserved_bytes > 0 {
        conn.execute(
            "UPDATE storage_usage
             SET reserved_bytes = MAX(0, reserved_bytes - ?1), updated_at = ?2
             WHERE user_id = ?3",
            params![
                i64::try_from(existing.reserved_bytes).unwrap_or(i64::MAX),
                now_str,
                user_id
            ],
        )?;
    }

    fetch_upload_for_owner(conn, user_id, session_id)
}

impl Database {
    /// Creates an upload session after atomically verifying and reserving user quota.
    pub async fn create_upload_session(
        &self,
        input: NewUploadSession,
    ) -> Result<UploadSessionRecord> {
        if input.part_size_bytes == 0 {
            return Err(DbError::InvalidInput(
                "part_size_bytes must be positive".into(),
            ));
        }
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (used, reserved, limit): (u64, u64, u64) = tx
                .query_row(
                    "SELECT su.used_bytes, su.reserved_bytes, u.quota_limit_bytes
                     FROM users u
                     INNER JOIN storage_usage su ON su.user_id = u.id
                     WHERE u.id = ?1 AND u.is_active = 1",
                    params![input.user_id],
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
                    DbError::NotFound(format!("active user '{}' not found", input.user_id))
                })?;

            if used
                .saturating_add(reserved)
                .saturating_add(input.total_bytes)
                > limit
            {
                return Err(DbError::QuotaExceeded {
                    used_bytes: used,
                    reserved_bytes: reserved,
                    requested_bytes: input.total_bytes,
                    limit_bytes: limit,
                });
            }

            let now = Utc::now();
            let now_str = now.to_rfc3339();
            let metadata_json = serde_json::to_string(&input.metadata)?;
            let total_i64 = i64::try_from(input.total_bytes)
                .map_err(|_| DbError::InvalidInput("total_bytes exceeds i64::MAX".into()))?;
            let part_size_i64 = i64::try_from(input.part_size_bytes)
                .map_err(|_| DbError::InvalidInput("part_size_bytes exceeds i64::MAX".into()))?;

            tx.execute(
                "UPDATE storage_usage
                 SET reserved_bytes = reserved_bytes + ?1, updated_at = ?2
                 WHERE user_id = ?3",
                params![total_i64, now_str, input.user_id],
            )?;

            tx.execute(
                "INSERT INTO uploads (
                    id, user_id, clip_id, status, title, filename, content_type, sha256,
                    visibility, metadata_json, storage_key, multipart_upload_id, total_bytes,
                    bytes_received, reserved_bytes, part_size_bytes, total_parts,
                    uploaded_parts_json, created_at, updated_at, expires_at
                 ) VALUES (
                    ?1, ?2, ?3, 'pending', ?4, ?5, ?6, ?7,
                    ?8, ?9, ?10, NULL, ?11,
                    0, ?11, ?12, ?13,
                    '[]', ?14, ?14, ?15
                 )",
                params![
                    input.session_id,
                    input.user_id,
                    input.clip_id.to_string(),
                    input.title,
                    input.filename,
                    input.content_type,
                    input.sha256,
                    visibility_to_str(input.visibility),
                    metadata_json,
                    input.storage_key,
                    total_i64,
                    part_size_i64,
                    i64::from(input.total_parts),
                    now_str,
                    input.expires_at.to_rfc3339(),
                ],
            )?;

            let session = fetch_upload_for_owner(&tx, &input.user_id, &input.session_id)?
                .ok_or_else(|| DbError::Internal("inserted upload session missing".into()))?;
            tx.commit()?;
            Ok(session)
        })
        .await
    }

    /// Fetches an upload session strictly owned by `user_id`.
    pub async fn get_upload_session_for_owner(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<UploadSessionRecord>> {
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| fetch_upload_for_owner(conn, &user_id, &session_id))
            .await
    }

    /// Associates a storage-backend multipart upload ID with an owned session.
    pub async fn set_upload_multipart_id(
        &self,
        user_id: &str,
        session_id: &str,
        multipart_upload_id: &str,
    ) -> Result<()> {
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        let multipart_upload_id = multipart_upload_id.to_string();
        self.with_conn(move |conn| {
            let now_str = Utc::now().to_rfc3339();
            let updated = conn.execute(
                "UPDATE uploads SET multipart_upload_id = ?1, updated_at = ?2
                 WHERE id = ?3 AND user_id = ?4",
                params![multipart_upload_id, now_str, session_id, user_id],
            )?;
            if updated == 0 {
                return Err(DbError::NotFound(format!(
                    "upload session '{session_id}' not found"
                )));
            }
            Ok(())
        })
        .await
    }

    /// Records a received part for an active upload session owned by `user_id`.
    pub async fn record_upload_part(
        &self,
        user_id: &str,
        session_id: &str,
        part_number: u32,
        part_bytes: u64,
    ) -> Result<UploadSessionRecord> {
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(session) = fetch_upload_for_owner(&tx, &user_id, &session_id)? else {
                return Err(DbError::NotFound(format!(
                    "upload session '{session_id}' not found"
                )));
            };

            if !matches!(
                session.status,
                UploadSessionStatus::Pending | UploadSessionStatus::Uploading
            ) {
                return Err(DbError::Conflict(format!(
                    "upload session '{session_id}' is in {:?} state",
                    session.status
                )));
            }
            if part_number == 0 || part_number > session.total_parts {
                return Err(DbError::InvalidInput(format!(
                    "part_number {part_number} out of range 1..={}",
                    session.total_parts
                )));
            }

            let mut parts = session.uploaded_parts;
            let mut bytes_received = session.bytes_received;
            if !parts.contains(&part_number) {
                parts.push(part_number);
                parts.sort_unstable();
                bytes_received = bytes_received.saturating_add(part_bytes);
            }

            let parts_json = serde_json::to_string(&parts)?;
            let now_str = Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE uploads
                 SET status = 'uploading', bytes_received = ?1, uploaded_parts_json = ?2, updated_at = ?3
                 WHERE id = ?4 AND user_id = ?5",
                params![
                    i64::try_from(bytes_received).unwrap_or(i64::MAX),
                    parts_json,
                    now_str,
                    session_id,
                    user_id,
                ],
            )?;

            let updated = fetch_upload_for_owner(&tx, &user_id, &session_id)?
                .ok_or_else(|| DbError::Internal("upload session vanished".into()))?;
            tx.commit()?;
            Ok(updated)
        })
        .await
    }

    /// Transitions an owned upload session into `Finalizing`.
    pub async fn mark_upload_finalizing(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<UploadSessionRecord> {
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(session) = fetch_upload_for_owner(&tx, &user_id, &session_id)? else {
                return Err(DbError::NotFound(format!(
                    "upload session '{session_id}' not found"
                )));
            };
            if !matches!(
                session.status,
                UploadSessionStatus::Pending | UploadSessionStatus::Uploading
            ) {
                return Err(DbError::Conflict(format!(
                    "upload session '{session_id}' cannot finalize from {:?} state",
                    session.status
                )));
            }
            let now_str = Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE uploads SET status = 'finalizing', updated_at = ?1 WHERE id = ?2 AND user_id = ?3",
                params![now_str, session_id, user_id],
            )?;
            let updated = fetch_upload_for_owner(&tx, &user_id, &session_id)?
                .ok_or_else(|| DbError::Internal("upload session vanished".into()))?;
            tx.commit()?;
            Ok(updated)
        })
        .await
    }

    /// Atomically completes an upload session: creates the `Clip` and `Original`
    /// `ClipAsset`, releases the quota reservation, and commits actual `used_bytes`.
    pub async fn complete_upload_session(
        &self,
        user_id: &str,
        session_id: &str,
        actual_size_bytes: u64,
        sha256: Option<String>,
    ) -> Result<(UploadSessionRecord, ClipDto)> {
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some(session) = fetch_upload_for_owner(&tx, &user_id, &session_id)? else {
                return Err(DbError::NotFound(format!(
                    "upload session '{session_id}' not found"
                )));
            };
            if !matches!(
                session.status,
                UploadSessionStatus::Pending
                    | UploadSessionStatus::Uploading
                    | UploadSessionStatus::Finalizing
            ) {
                return Err(DbError::Conflict(format!(
                    "upload session '{session_id}' is already {:?}",
                    session.status
                )));
            }

            let (used, reserved, limit): (u64, u64, u64) = tx.query_row(
                "SELECT su.used_bytes, su.reserved_bytes, u.quota_limit_bytes
                 FROM users u
                 INNER JOIN storage_usage su ON su.user_id = u.id
                 WHERE u.id = ?1",
                params![user_id],
                |row| {
                    Ok((
                        u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                        u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                        u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                    ))
                },
            )?;

            let remaining_reserved = reserved.saturating_sub(session.reserved_bytes);
            if used
                .saturating_add(remaining_reserved)
                .saturating_add(actual_size_bytes)
                > limit
            {
                return Err(DbError::QuotaExceeded {
                    used_bytes: used,
                    reserved_bytes: remaining_reserved,
                    requested_bytes: actual_size_bytes,
                    limit_bytes: limit,
                });
            }

            let now = Utc::now();
            let new_clip = NewClip {
                id: session.clip_id,
                owner_id: user_id.clone(),
                title: session.title.clone(),
                game: session.metadata.game.clone(),
                duration_ms: session.metadata.duration_ms,
                width: session.metadata.width,
                height: session.metadata.height,
                fps: session.metadata.fps,
                size_bytes: actual_size_bytes,
                codec: session.metadata.codec.clone(),
                content_type: session.content_type.clone(),
                audio_tracks: session.metadata.audio_tracks.clone(),
                tags: session.metadata.tags.clone(),
                notes: session.metadata.notes.clone(),
                visibility: session.visibility,
                created_at: session.metadata.created_at.unwrap_or(now),
                uploaded_at: now,
                expires_at: None,
            };
            insert_clip_row(&tx, &new_clip)?;

            let final_sha = sha256.or_else(|| session.sha256.clone());
            let new_asset = NewClipAsset {
                id: Uuid::new_v4().to_string(),
                clip_id: session.clip_id,
                owner_id: user_id.clone(),
                asset_kind: ClipAssetKind::Original,
                storage_key: session.storage_key.clone(),
                content_type: session.content_type.clone(),
                size_bytes: actual_size_bytes,
                sha256: final_sha.clone(),
            };
            insert_asset_row(&tx, &new_asset)?;

            let now_str = now.to_rfc3339();
            tx.execute(
                "UPDATE storage_usage
                 SET reserved_bytes = MAX(0, reserved_bytes - ?1),
                     used_bytes = used_bytes + ?2,
                     clip_count = clip_count + 1,
                     updated_at = ?3
                 WHERE user_id = ?4",
                params![
                    i64::try_from(session.reserved_bytes).unwrap_or(i64::MAX),
                    i64::try_from(actual_size_bytes).unwrap_or(i64::MAX),
                    now_str,
                    user_id,
                ],
            )?;

            tx.execute(
                "UPDATE uploads
                 SET status = 'completed', reserved_bytes = 0, bytes_received = ?1,
                     sha256 = COALESCE(?2, sha256), updated_at = ?3
                 WHERE id = ?4 AND user_id = ?5",
                params![
                    i64::try_from(actual_size_bytes).unwrap_or(i64::MAX),
                    final_sha,
                    now_str,
                    session_id,
                    user_id,
                ],
            )?;

            let completed_session = fetch_upload_for_owner(&tx, &user_id, &session_id)?
                .ok_or_else(|| DbError::Internal("upload session vanished".into()))?;
            let clip_dto = fetch_clip_by_id_and_owner(&tx, &user_id, session.clip_id)?
                .ok_or_else(|| DbError::Internal("created clip vanished".into()))?;

            tx.commit()?;
            Ok((completed_session, clip_dto))
        })
        .await
    }

    /// Marks an owned upload session as failed and releases its quota reservation.
    pub async fn fail_upload_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<UploadSessionRecord>> {
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let res = release_upload_reservation(
                &tx,
                &user_id,
                &session_id,
                UploadSessionStatus::Failed,
            )?;
            tx.commit()?;
            Ok(res)
        })
        .await
    }

    /// Aborts an owned upload session and releases its quota reservation.
    pub async fn abort_upload_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<Option<UploadSessionRecord>> {
        let user_id = user_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let res = release_upload_reservation(
                &tx,
                &user_id,
                &session_id,
                UploadSessionStatus::Aborted,
            )?;
            tx.commit()?;
            Ok(res)
        })
        .await
    }

    /// Finds all expired active upload sessions, releases their quota reservations,
    /// marks them `Failed`, and returns them for storage cleanup.
    pub async fn cleanup_expired_uploads(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<UploadSessionRecord>> {
        let now_str = now.to_rfc3339();
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let sql = format!(
                "SELECT {UPLOAD_SELECT_COLS} FROM uploads
                 WHERE status IN ('pending', 'uploading', 'finalizing') AND expires_at <= ?1"
            );
            let expired = {
                let mut stmt = tx.prepare(&sql)?;
                stmt.query_map(params![now_str], map_upload_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };

            let mut cleaned = Vec::with_capacity(expired.len());
            for session in expired {
                if let Some(updated) = release_upload_reservation(
                    &tx,
                    &session.user_id,
                    &session.session_id,
                    UploadSessionStatus::Failed,
                )? {
                    cleaned.push(updated);
                }
            }
            tx.commit()?;
            Ok(cleaned)
        })
        .await
    }

    /// Returns a user's current storage usage and remaining quota.
    pub async fn get_storage_usage(&self, user_id: &str) -> Result<QuotaDto> {
        let user_id = user_id.to_string();
        self.with_conn(move |conn| {
            let (used_bytes, reserved_bytes, clip_count, limit_bytes): (u64, u64, u64, u64) = conn
                .query_row(
                    "SELECT su.used_bytes, su.reserved_bytes, su.clip_count, u.quota_limit_bytes
                     FROM users u
                     INNER JOIN storage_usage su ON su.user_id = u.id
                     WHERE u.id = ?1",
                    params![user_id],
                    |row| {
                        Ok((
                            u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                            u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                            u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                            u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| DbError::NotFound(format!("user '{user_id}' not found")))?;

            let effective_used = used_bytes.saturating_add(reserved_bytes);
            Ok(QuotaDto {
                used_bytes,
                limit_bytes,
                remaining_bytes: limit_bytes.saturating_sub(effective_used),
                clip_count,
            })
        })
        .await
    }
}
