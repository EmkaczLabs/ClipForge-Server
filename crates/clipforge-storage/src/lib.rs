//! Object storage abstraction for ClipForge Server (Local disk & S3/RustFS).
//!
//! Owned by `@srv-storage`. Supports PutObject, GetObject, HeadObject,
//! DeleteObject, byte-range streaming, and S3 multipart uploads via `reqwest`
//! with AWS SigV4 (`ring` HMAC-SHA256 / SHA-256).

use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use thiserror::Error;

/// Stream of bytes returned by object storage reads.
pub type ByteStream =
    Pin<Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>>;

/// Errors returned by object storage backends.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("invalid storage key: {0}")]
    InvalidKey(String),
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http/s3 error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("storage backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Metadata for a stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub size_bytes: u64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
}

/// Configuration for selecting an object storage backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageConfig {
    Local {
        root_dir: PathBuf,
    },
    S3 {
        endpoint: String,
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
        force_path_style: bool,
    },
}

/// Object storage trait implemented by Local FS and S3/RustFS backends.
#[async_trait]
pub trait ObjectStorage: Send + Sync {
    /// Streams a file from `source_path` into storage under `key`.
    async fn put_file(
        &self,
        key: &str,
        source_path: &Path,
        size_bytes: u64,
        content_type: &str,
    ) -> Result<()>;

    /// Uploads raw bytes under `key`.
    async fn put_bytes(&self, key: &str, data: Bytes, content_type: &str) -> Result<()>;

    /// Returns object size and optional content-type.
    async fn head(&self, key: &str) -> Result<ObjectMeta>;

    /// Streams an inclusive byte range `[start, end_inclusive]` for `key`.
    async fn get_range(&self, key: &str, start: u64, end_inclusive: u64) -> Result<ByteStream>;

    /// Deletes an object by key (succeeds if already absent).
    async fn delete(&self, key: &str) -> Result<()>;

    /// Verifies connectivity and bucket/directory accessibility.
    async fn health_check(&self) -> Result<()>;
}

/// Validates an object key so it cannot escape the storage root.
pub fn validate_storage_key(key: &str) -> Result<&str> {
    if key.is_empty()
        || key.starts_with('/')
        || key.contains('\\')
        || key.contains('\0')
        || key.contains("..")
    {
        return Err(StorageError::InvalidKey(key.to_string()));
    }
    for comp in Path::new(key).components() {
        if !matches!(comp, Component::Normal(_)) {
            return Err(StorageError::InvalidKey(key.to_string()));
        }
    }
    Ok(key)
}
