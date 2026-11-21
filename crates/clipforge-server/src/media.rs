//! Media thumbnail extraction and fallback image helpers.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use clipforge_db::{ClipAssetKind, NewClipAsset};
use clipforge_storage::{ClipObjectKind, build_clip_object_key};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;

use crate::routes::AppState;

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(15);

/// Valid 1x1 `#121318` PNG returned when no generated thumbnail exists.
pub const FALLBACK_THUMBNAIL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x10, 0x12, 0x96, 0x00,
    0x00, 0x00, 0x78, 0x00, 0x3e, 0x04, 0xe2, 0xde, 0x87, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

async fn run_ffmpeg_frame(video_path: &Path, output_path: &Path, seek_ts: &str) -> bool {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-ss")
        .arg(seek_ts)
        .arg("-i")
        .arg(video_path)
        .arg("-frames:v")
        .arg("1")
        .arg("-q:v")
        .arg("3")
        .arg("-vf")
        .arg("scale='min(1280,iw)':-1")
        .arg("-y")
        .arg(output_path)
        .kill_on_drop(true);

    match tokio::time::timeout(FFMPEG_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out.status.success(),
        Ok(Err(err)) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                tracing::warn!("ffmpeg binary not found; skipping thumbnail generation");
            } else {
                tracing::warn!(error = %err, seek = seek_ts, "failed to spawn ffmpeg");
            }
            false
        }
        Err(_) => {
            tracing::warn!(
                seek = seek_ts,
                timeout_secs = FFMPEG_TIMEOUT.as_secs(),
                "ffmpeg thumbnail extraction timed out"
            );
            false
        }
    }
}

/// Extracts a representative JPEG thumbnail from a local video file.
///
/// Tries `00:00:01` first, then falls back to `00:00:00` for short clips.
/// Returns `None` if `ffmpeg` is unavailable, times out, or fails.
pub async fn extract_thumbnail_jpeg(video_path: &Path) -> Option<Vec<u8>> {
    let out_file = match tempfile::Builder::new().suffix(".jpg").tempfile() {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(error = %err, "failed to create temp file for thumbnail");
            return None;
        }
    };
    let out_path = out_file.path().to_path_buf();

    for seek_ts in ["00:00:01", "00:00:00"] {
        if run_ffmpeg_frame(video_path, &out_path, seek_ts).await
            && let Ok(bytes) = tokio::fs::read(&out_path).await
            && !bytes.is_empty()
        {
            return Some(bytes);
        }
    }

    None
}

/// Generates a JPEG thumbnail from `video_path`, uploads it to storage, and records it in DB.
///
/// Errors are logged and ignored so thumbnail generation never fails a clip upload.
pub async fn generate_and_store_thumbnail(
    state: &AppState,
    user_id: &str,
    clip_id: Uuid,
    video_path: &Path,
) {
    let Some(jpeg_bytes) = extract_thumbnail_jpeg(video_path).await else {
        return;
    };

    let clip_id_str = clip_id.to_string();
    let thumb_key = match build_clip_object_key(user_id, &clip_id_str, ClipObjectKind::Thumbnail) {
        Ok(k) => k,
        Err(err) => {
            tracing::warn!(clip_id = %clip_id, error = %err, "invalid thumbnail storage key");
            return;
        }
    };

    let size_bytes = jpeg_bytes.len() as u64;
    if let Err(err) = state
        .storage
        .put_bytes(&thumb_key, Bytes::from(jpeg_bytes), "image/jpeg")
        .await
    {
        tracing::warn!(
            clip_id = %clip_id,
            storage_key = %thumb_key,
            error = %err,
            "failed to store generated thumbnail"
        );
        return;
    }

    let asset = NewClipAsset {
        id: Uuid::new_v4().to_string(),
        clip_id,
        owner_id: user_id.to_string(),
        asset_kind: ClipAssetKind::Thumbnail,
        storage_key: thumb_key.clone(),
        content_type: "image/jpeg".to_string(),
        size_bytes,
        sha256: None,
    };

    if let Err(err) = state.db.upsert_clip_asset_for_owner(user_id, asset).await {
        tracing::warn!(
            clip_id = %clip_id,
            error = %err,
            "failed to record thumbnail asset in database; cleaning up storage object"
        );
        let _ = state.storage.delete_object(&thumb_key).await;
    }
}

/// Streams a stored video object to a temp file and generates/stores its JPEG thumbnail.
///
/// Errors are logged and ignored so thumbnail generation never fails upload completion.
pub async fn generate_thumbnail_from_storage(
    state: &AppState,
    user_id: &str,
    clip_id: Uuid,
    storage_key: &str,
) {
    let range_obj = match state.storage.get_object_range(storage_key, 0, None).await {
        Ok(obj) => obj,
        Err(err) => {
            tracing::warn!(
                clip_id = %clip_id,
                storage_key = %storage_key,
                error = %err,
                "failed to read uploaded video from storage for thumbnail generation"
            );
            return;
        }
    };

    let named = match tempfile::Builder::new().suffix(".mp4").tempfile() {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(clip_id = %clip_id, error = %err, "failed to create temp video file");
            return;
        }
    };
    let std_file = match named.reopen() {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(clip_id = %clip_id, error = %err, "failed to reopen temp video file");
            return;
        }
    };
    let mut async_file = tokio::fs::File::from_std(std_file);
    let mut stream = range_obj.stream;
    while let Some(chunk_res) = stream.next().await {
        match chunk_res {
            Ok(chunk) => {
                if let Err(err) = async_file.write_all(&chunk).await {
                    tracing::warn!(clip_id = %clip_id, error = %err, "failed to write temp video chunk");
                    return;
                }
            }
            Err(err) => {
                tracing::warn!(clip_id = %clip_id, error = %err, "failed to stream video for thumbnail");
                return;
            }
        }
    }
    if let Err(err) = async_file.flush().await {
        tracing::warn!(clip_id = %clip_id, error = %err, "failed to flush temp video file");
        return;
    }

    generate_and_store_thumbnail(state, user_id, clip_id, named.path()).await;
}
