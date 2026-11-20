//! HTTP `Range: bytes=...` parsing and streaming response helpers.

use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use clipforge_storage::StorageBackend;

use crate::error::ServerError;

/// Parses an HTTP `Range: bytes=...` header into an inclusive `(start, end)` pair.
pub fn parse_byte_range(header_val: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = header_val.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (start_s, end_s) = first.split_once('-')?;

    if start_s.is_empty() {
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = total.saturating_sub(suffix);
        Some((start, total - 1))
    } else {
        let start: u64 = start_s.parse().ok()?;
        if start >= total {
            return None;
        }
        let end = if end_s.is_empty() {
            total - 1
        } else {
            let parsed_end: u64 = end_s.parse().ok()?;
            if parsed_end < start {
                return None;
            }
            parsed_end.min(total - 1)
        };
        Some((start, end))
    }
}

/// Responds to `HEAD` requests for a stored media object.
pub async fn head_object_response(
    storage: &dyn StorageBackend,
    storage_key: &str,
    fallback_content_type: &str,
) -> Result<Response, ServerError> {
    let meta = storage.head_object(storage_key).await?;
    let content_type = meta
        .content_type
        .unwrap_or_else(|| fallback_content_type.to_string());
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_LENGTH, meta.size_bytes.to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
        ],
    )
        .into_response())
}

/// Responds to `GET` requests for a stored media object with full HTTP `Range` support.
pub async fn stream_object_response(
    storage: &dyn StorageBackend,
    storage_key: &str,
    fallback_content_type: &str,
    headers: &HeaderMap,
) -> Result<Response, ServerError> {
    let meta = storage.head_object(storage_key).await?;
    let total = meta.size_bytes;
    let content_type = meta
        .content_type
        .unwrap_or_else(|| fallback_content_type.to_string());

    if total == 0 {
        return Err(ServerError::NotFound("empty media object".to_string()));
    }

    if let Some(range_str) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        let (start, end) = parse_byte_range(range_str, total)
            .ok_or(ServerError::RangeNotSatisfiable { total_bytes: total })?;
        let chunk_len = end - start + 1;
        let range_obj = storage
            .get_object_range(storage_key, start, Some(end))
            .await?;

        Ok((
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, content_type),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_LENGTH, chunk_len.to_string()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                ),
            ],
            Body::from_stream(range_obj.stream),
        )
            .into_response())
    } else {
        let range_obj = storage
            .get_object_range(storage_key, 0, Some(total - 1))
            .await?;

        Ok((
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_LENGTH, total.to_string()),
            ],
            Body::from_stream(range_obj.stream),
        )
            .into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_byte_range;

    #[test]
    fn byte_range_parsing() {
        assert_eq!(parse_byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_byte_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_byte_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_byte_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_byte_range("bytes=1000-1005", 1000), None);
        assert_eq!(parse_byte_range("bytes=200-100", 1000), None);
        assert_eq!(parse_byte_range("bytes=0-0", 0), None);
    }
}
