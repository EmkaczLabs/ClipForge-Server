//! Object storage abstraction for ClipForge Server (Local disk & S3/RustFS).
//!
//! Owned by `@srv-storage`. Supports streaming PutObject, Range GetObject,
//! HeadObject, DeleteObject, multipart upload/abort/cleanup, AWS SigV4
//! (`ring` HMAC-SHA256 / SHA-256 + `hex` + `reqwest`), and non-destructive
//! storage diagnostics (`run_storage_check`).

pub mod check;
pub mod key;
pub mod local;
pub mod s3;
pub mod sigv4;

use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use thiserror::Error;

pub use check::{
    CapabilityStatus, CheckStatus, StorageCapabilities, StorageCheckItem, StorageCheckOptions,
    StorageCheckReport, run_storage_check,
};
pub use key::{ClipObjectKind, build_clip_object_key, validate_id_segment, validate_storage_key};
pub use local::LocalStorage;
pub use s3::{S3Config, S3Storage};

/// Stream of bytes returned by object storage reads or accepted by streaming writes.
pub type ByteStream =
    Pin<Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>>;

/// Errors returned by object storage backends.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("invalid storage key: {0}")]
    InvalidKey(String),
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("invalid byte range: {0}")]
    InvalidRange(String),
    #[error(
        "insufficient disk space: available {available_bytes} bytes, required {required_bytes} bytes"
    )]
    InsufficientSpace {
        available_bytes: u64,
        required_bytes: u64,
    },
    #[error("storage backend unavailable: {0}")]
    Unavailable(String),
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

/// Response for an HTTP Range object read.
pub struct RangeObject {
    pub stream: ByteStream,
    pub total_size: u64,
    pub start: u64,
    pub end_inclusive: u64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
}

/// Handle returned when initiating a multipart upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartUpload {
    pub key: String,
    pub upload_id: String,
}

/// Part receipt returned by [`StorageBackend::upload_part`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedPart {
    pub part_number: u32,
    pub etag: String,
}

/// Configuration for selecting an object storage backend.
#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { root_dir } => f
                .debug_struct("StorageConfig::Local")
                .field("root_dir", root_dir)
                .finish(),
            Self::S3 {
                endpoint,
                bucket,
                region,
                access_key,
                secret_key: _,
                force_path_style,
            } => f
                .debug_struct("StorageConfig::S3")
                .field("endpoint", endpoint)
                .field("bucket", bucket)
                .field("region", region)
                .field(
                    "access_key",
                    &if access_key.is_empty() {
                        ""
                    } else {
                        "[REDACTED]"
                    },
                )
                .field("secret_key", &"[REDACTED]")
                .field("force_path_style", force_path_style)
                .finish(),
        }
    }
}

/// Object storage trait implemented by Local FS and S3/RustFS backends.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Streams bytes from `stream` into storage under `key` with bounded buffering.
    async fn put_object(
        &self,
        key: &str,
        stream: ByteStream,
        size_bytes: Option<u64>,
        content_type: &str,
    ) -> Result<ObjectMeta>;

    /// Uploads raw bytes under `key`.
    async fn put_bytes(&self, key: &str, data: Bytes, content_type: &str) -> Result<ObjectMeta>;

    /// Streams a file from `source_path` into storage under `key`.
    async fn put_file(
        &self,
        key: &str,
        source_path: &Path,
        size_bytes: u64,
        content_type: &str,
    ) -> Result<ObjectMeta>;

    /// Streams an inclusive byte range `[start, end_inclusive]` (or `[start, EOF]` if `None`).
    async fn get_object_range(
        &self,
        key: &str,
        start: u64,
        end_inclusive: Option<u64>,
    ) -> Result<RangeObject>;

    /// Returns object size, content-type, and optional ETag.
    async fn head_object(&self, key: &str) -> Result<ObjectMeta>;

    /// Deletes an object by key (succeeds if already absent).
    async fn delete_object(&self, key: &str) -> Result<()>;

    /// Initiates a multipart upload for `key`.
    async fn create_multipart(&self, key: &str, content_type: &str) -> Result<MultipartUpload>;

    /// Uploads a single part (`1..=10000`) for an active multipart upload.
    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        data: Bytes,
    ) -> Result<CompletedPart>;

    /// Completes a multipart upload and commits the final object.
    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[CompletedPart],
    ) -> Result<ObjectMeta>;

    /// Aborts a multipart upload and cleans up staged parts.
    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()>;

    /// Verifies connectivity and bucket/directory accessibility.
    async fn health_check(&self) -> Result<()>;
}

/// Instantiates a [`StorageBackend`] from [`StorageConfig`].
pub fn create_storage_backend(config: &StorageConfig) -> Result<Arc<dyn StorageBackend>> {
    match config {
        StorageConfig::Local { root_dir } => Ok(Arc::new(LocalStorage::new(root_dir))),
        StorageConfig::S3 {
            endpoint,
            bucket,
            region,
            access_key,
            secret_key,
            force_path_style,
        } => {
            let s3_cfg = S3Config {
                endpoint: endpoint.clone(),
                bucket: bucket.clone(),
                region: region.clone(),
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                force_path_style: *force_path_style,
                connect_timeout: Duration::from_secs(10),
                read_timeout: Duration::from_secs(30),
            };
            Ok(Arc::new(S3Storage::new(s3_cfg)?))
        }
    }
}
