//! Server-generated object keys and path-traversal validation.

use std::path::{Component, Path};

use crate::{Result, StorageError};

/// Supported asset kinds stored for a clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipObjectKind {
    Original,
    Thumbnail,
    Preview,
}

impl ClipObjectKind {
    /// Returns the fixed server-assigned filename for this asset kind.
    pub const fn filename(self) -> &'static str {
        match self {
            Self::Original => "original.mp4",
            Self::Thumbnail => "thumbnail.webp",
            Self::Preview => "preview.mp4",
        }
    }

    /// Returns the default MIME content-type for this asset kind.
    pub const fn default_content_type(self) -> &'static str {
        match self {
            Self::Original | Self::Preview => "video/mp4",
            Self::Thumbnail => "image/webp",
        }
    }
}

/// Validates an identifier segment (`user_id` or `clip_id`) used in server-generated keys.
///
/// Only ASCII alphanumeric characters, `-`, and `_` are allowed (1..=128 bytes).
pub fn validate_id_segment(segment: &str) -> Result<&str> {
    if segment.is_empty() || segment.len() > 128 {
        return Err(StorageError::InvalidKey(format!(
            "invalid identifier length: {}",
            segment.len()
        )));
    }
    if !segment
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(StorageError::InvalidKey(format!(
            "identifier contains disallowed characters: {segment}"
        )));
    }
    Ok(segment)
}

/// Builds a canonical server-generated object key: `clips/{user_id}/{clip_id}/{filename}`.
pub fn build_clip_object_key(user_id: &str, clip_id: &str, kind: ClipObjectKind) -> Result<String> {
    let uid = validate_id_segment(user_id)?;
    let cid = validate_id_segment(clip_id)?;
    Ok(format!("clips/{uid}/{cid}/{}", kind.filename()))
}

/// Validates an object key so it cannot escape the storage root or target internal metadata.
pub fn validate_storage_key(key: &str) -> Result<&str> {
    if key.is_empty()
        || key.starts_with('/')
        || key.ends_with('/')
        || key.contains("//")
        || key.contains('\\')
        || key.contains('\0')
        || key.contains("..")
        || key.bytes().any(|b| b.is_ascii_control())
    {
        return Err(StorageError::InvalidKey(key.to_string()));
    }

    let mut segments = key.split('/');
    if let Some(first) = segments.next()
        && matches!(first, ".tmp" | ".meta" | ".multipart")
    {
        return Err(StorageError::InvalidKey(format!(
            "reserved storage prefix: {key}"
        )));
    }

    for seg in key.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(StorageError::InvalidKey(key.to_string()));
        }
    }

    for comp in Path::new(key).components() {
        if !matches!(comp, Component::Normal(_)) {
            return Err(StorageError::InvalidKey(key.to_string()));
        }
    }
    Ok(key)
}
