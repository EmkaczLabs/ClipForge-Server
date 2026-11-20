//! Local filesystem object storage backend with atomic writes,
//! directory permissions, disk-space checks, streaming Range reads,
//! and multipart upload staging/cleanup.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio_util::io::ReaderStream;
use tracing::warn;
use uuid::Uuid;

use crate::key::validate_storage_key;
use crate::sigv4::sha256_hex;
use crate::{
    ByteStream, CompletedPart, MultipartUpload, ObjectMeta, RangeObject, Result, StorageBackend,
    StorageError,
};

const STREAM_BUF_SIZE: usize = 64 * 1024;
const DEFAULT_MIN_FREE_BYTES: u64 = 32 * 1024 * 1024; // 32 MiB safety buffer

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalObjectMetadata {
    content_type: String,
    etag: String,
    size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MultipartManifest {
    key: String,
    content_type: String,
}

/// Local filesystem implementation of [`StorageBackend`].
#[derive(Debug, Clone)]
pub struct LocalStorage {
    root_dir: PathBuf,
    min_free_bytes: u64,
}

impl LocalStorage {
    /// Creates a new [`LocalStorage`] rooted at `root_dir` with a 32 MiB minimum free space guard.
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
            min_free_bytes: DEFAULT_MIN_FREE_BYTES,
        }
    }

    /// Overrides the minimum free disk space buffer required before writes.
    pub fn with_min_free_bytes(mut self, min_free_bytes: u64) -> Self {
        self.min_free_bytes = min_free_bytes;
        self
    }

    /// Returns the configured root directory.
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root_dir.join(".tmp")
    }

    fn meta_dir(&self) -> PathBuf {
        self.root_dir.join(".meta")
    }

    fn multipart_dir(&self) -> PathBuf {
        self.root_dir.join(".multipart")
    }

    fn object_path(&self, key: &str) -> Result<PathBuf> {
        let key = validate_storage_key(key)?;
        Ok(self.root_dir.join(key))
    }

    fn meta_path(&self, key: &str) -> Result<PathBuf> {
        let key = validate_storage_key(key)?;
        Ok(self.meta_dir().join(format!("{key}.json")))
    }

    fn upload_dir(&self, upload_id: &str) -> Result<PathBuf> {
        if upload_id.is_empty()
            || upload_id.contains('/')
            || upload_id.contains('\\')
            || upload_id.contains("..")
            || !upload_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(StorageError::InvalidKey(format!(
                "invalid upload_id: {upload_id}"
            )));
        }
        Ok(self.multipart_dir().join(upload_id))
    }

    async fn ensure_base_dirs(&self) -> Result<()> {
        create_dir_all_secure(&self.root_dir).await?;
        create_dir_all_secure(&self.tmp_dir()).await?;
        create_dir_all_secure(&self.meta_dir()).await?;
        create_dir_all_secure(&self.multipart_dir()).await?;
        Ok(())
    }

    async fn check_disk_space(&self, required_bytes: u64) -> Result<()> {
        let check_path = if self.root_dir.exists() {
            self.root_dir.clone()
        } else {
            self.root_dir
                .ancestors()
                .find(|p| p.exists())
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf()
        };

        let output = match tokio::process::Command::new("df")
            .arg("-k")
            .arg("-P")
            .arg(&check_path)
            .output()
            .await
        {
            Ok(out) if out.status.success() => out,
            Ok(out) => {
                warn!(
                    status = ?out.status,
                    "df disk-space check exited non-zero; skipping strict check"
                );
                return Ok(());
            }
            Err(err) => {
                warn!(error = %err, "failed to run df for disk-space check");
                return Ok(());
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        // POSIX `df -k -P` outputs header on line 1 and columns:
        // Filesystem 1024-blocks Used Available Capacity Mounted-on
        if let Some(data_line) = stdout.lines().nth(1) {
            let cols: Vec<&str> = data_line.split_whitespace().collect();
            if cols.len() >= 4
                && let Ok(avail_kb) = cols[3].parse::<u64>()
            {
                let available_bytes = avail_kb.saturating_mul(1024);
                let needed = required_bytes.saturating_add(self.min_free_bytes);
                if available_bytes < needed {
                    return Err(StorageError::InsufficientSpace {
                        available_bytes,
                        required_bytes: needed,
                    });
                }
            }
        }
        Ok(())
    }

    async fn verify_no_symlinks(&self, target: &Path) -> Result<()> {
        let rel = target
            .strip_prefix(&self.root_dir)
            .map_err(|_| StorageError::InvalidKey(target.display().to_string()))?;
        let mut cur = self.root_dir.clone();
        for comp in rel.components() {
            cur.push(comp);
            match fs::symlink_metadata(&cur).await {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        return Err(StorageError::InvalidKey(format!(
                            "symlink traversal not allowed: {}",
                            cur.display()
                        )));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn write_metadata(&self, key: &str, meta: &LocalObjectMetadata) -> Result<()> {
        let meta_path = self.meta_path(key)?;
        if let Some(parent) = meta_path.parent() {
            create_dir_all_secure(parent).await?;
        }
        let tmp_meta = self.tmp_dir().join(format!("{}.meta.json", Uuid::new_v4()));
        let bytes = serde_json::to_vec(meta)
            .map_err(|e| StorageError::Backend(format!("metadata serialize error: {e}")))?;
        fs::write(&tmp_meta, &bytes).await?;
        set_file_permissions_unix(&tmp_meta, 0o640).await?;
        fs::rename(&tmp_meta, &meta_path).await?;
        Ok(())
    }

    async fn read_metadata(&self, key: &str) -> Result<Option<LocalObjectMetadata>> {
        let meta_path = self.meta_path(key)?;
        match fs::read(&meta_path).await {
            Ok(bytes) => {
                let parsed: LocalObjectMetadata = serde_json::from_slice(&bytes)
                    .map_err(|e| StorageError::Backend(format!("metadata parse error: {e}")))?;
                Ok(Some(parsed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Removes incomplete temporary files and stale multipart upload directories older than `max_age`.
    pub async fn cleanup_incomplete_uploads(&self, max_age: Duration) -> Result<usize> {
        let mut removed = 0usize;
        let now = SystemTime::now();

        for dir in [self.tmp_dir(), self.multipart_dir()] {
            let mut entries = match fs::read_dir(&dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };

            while let Some(entry) = entries.next_entry().await? {
                let meta = match entry.metadata().await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let modified = meta.modified().unwrap_or(now);
                let age = now.duration_since(modified).unwrap_or_default();
                if age >= max_age {
                    let path = entry.path();
                    if meta.is_dir() {
                        if fs::remove_dir_all(&path).await.is_ok() {
                            removed += 1;
                        }
                    } else if fs::remove_file(&path).await.is_ok() {
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }
}

#[async_trait]
impl StorageBackend for LocalStorage {
    async fn put_object(
        &self,
        key: &str,
        mut stream: ByteStream,
        size_bytes: Option<u64>,
        content_type: &str,
    ) -> Result<ObjectMeta> {
        let target_path = self.object_path(key)?;
        self.ensure_base_dirs().await?;
        self.verify_no_symlinks(&target_path).await?;
        self.check_disk_space(size_bytes.unwrap_or(0)).await?;

        if let Some(parent) = target_path.parent() {
            create_dir_all_secure(parent).await?;
        }

        let tmp_path = self.tmp_dir().join(format!("{}.part", Uuid::new_v4()));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .await?;
        let mut writer = BufWriter::with_capacity(STREAM_BUF_SIZE, file);
        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        let mut written: u64 = 0;

        let write_res: Result<()> = async {
            while let Some(chunk_res) = stream.next().await {
                let chunk = chunk_res?;
                if let Some(expected) = size_bytes
                    && written.saturating_add(chunk.len() as u64) > expected
                {
                    return Err(StorageError::Backend(format!(
                        "stream exceeded declared size {expected}"
                    )));
                }
                hasher.update(&chunk);
                writer.write_all(&chunk).await?;
                written = written.saturating_add(chunk.len() as u64);
            }
            writer.flush().await?;
            writer.get_ref().sync_all().await?;
            Ok(())
        }
        .await;

        if let Err(err) = write_res {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(err);
        }

        if let Some(expected) = size_bytes
            && written != expected
        {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(StorageError::Backend(format!(
                "stream size mismatch: expected {expected}, got {written}"
            )));
        }

        set_file_permissions_unix(&tmp_path, 0o640).await?;
        let etag = hex::encode(hasher.finish().as_ref());
        let meta = LocalObjectMetadata {
            content_type: content_type.to_string(),
            etag: etag.clone(),
            size_bytes: written,
        };
        self.write_metadata(key, &meta).await?;
        fs::rename(&tmp_path, &target_path).await?;

        Ok(ObjectMeta {
            size_bytes: written,
            content_type: Some(meta.content_type),
            etag: Some(etag),
        })
    }

    async fn put_bytes(&self, key: &str, data: Bytes, content_type: &str) -> Result<ObjectMeta> {
        let len = data.len() as u64;
        let stream: ByteStream = Box::pin(futures_util::stream::once(async move { Ok(data) }));
        self.put_object(key, stream, Some(len), content_type).await
    }

    async fn put_file(
        &self,
        key: &str,
        source_path: &Path,
        size_bytes: u64,
        content_type: &str,
    ) -> Result<ObjectMeta> {
        let file = File::open(source_path).await?;
        let stream: ByteStream = Box::pin(ReaderStream::with_capacity(file, STREAM_BUF_SIZE));
        self.put_object(key, stream, Some(size_bytes), content_type)
            .await
    }

    async fn get_object_range(
        &self,
        key: &str,
        start: u64,
        end_inclusive: Option<u64>,
    ) -> Result<RangeObject> {
        let path = self.object_path(key)?;
        self.verify_no_symlinks(&path).await?;
        let fs_meta = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(key.to_string()));
            }
            Err(e) => return Err(e.into()),
        };

        let total_size = fs_meta.len();
        let stored_meta = self.read_metadata(key).await?;
        let content_type = stored_meta.as_ref().map(|m| m.content_type.clone());
        let etag = stored_meta.as_ref().map(|m| m.etag.clone());

        if total_size == 0 {
            if start == 0 && end_inclusive.is_none() {
                let empty: ByteStream = Box::pin(futures_util::stream::empty());
                return Ok(RangeObject {
                    stream: empty,
                    total_size: 0,
                    start: 0,
                    end_inclusive: 0,
                    content_type,
                    etag,
                });
            }
            return Err(StorageError::InvalidRange(format!(
                "range {start}-{end_inclusive:?} on empty object"
            )));
        }

        if start >= total_size {
            return Err(StorageError::InvalidRange(format!(
                "range start {start} >= total size {total_size}"
            )));
        }

        let end = end_inclusive.unwrap_or(total_size - 1).min(total_size - 1);
        if end < start {
            return Err(StorageError::InvalidRange(format!(
                "range end {end} < start {start}"
            )));
        }

        let len = end - start + 1;
        let mut file = File::open(&path).await?;
        file.seek(SeekFrom::Start(start)).await?;
        let limited = file.take(len);
        let stream: ByteStream = Box::pin(ReaderStream::with_capacity(limited, STREAM_BUF_SIZE));

        Ok(RangeObject {
            stream,
            total_size,
            start,
            end_inclusive: end,
            content_type,
            etag,
        })
    }

    async fn head_object(&self, key: &str) -> Result<ObjectMeta> {
        let path = self.object_path(key)?;
        self.verify_no_symlinks(&path).await?;
        let fs_meta = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(key.to_string()));
            }
            Err(e) => return Err(e.into()),
        };

        let stored_meta = self.read_metadata(key).await?;
        Ok(ObjectMeta {
            size_bytes: fs_meta.len(),
            content_type: stored_meta.as_ref().map(|m| m.content_type.clone()),
            etag: stored_meta.as_ref().map(|m| m.etag.clone()),
        })
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        let path = self.object_path(key)?;
        match fs::remove_file(&path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let meta_path = self.meta_path(key)?;
        let _ = fs::remove_file(&meta_path).await;
        Ok(())
    }

    async fn create_multipart(&self, key: &str, content_type: &str) -> Result<MultipartUpload> {
        let key = validate_storage_key(key)?;
        self.ensure_base_dirs().await?;

        let upload_id = Uuid::new_v4().to_string();
        let dir = self.upload_dir(&upload_id)?;
        create_dir_all_secure(&dir).await?;

        let manifest = MultipartManifest {
            key: key.to_string(),
            content_type: content_type.to_string(),
        };
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|e| StorageError::Backend(format!("manifest serialize error: {e}")))?;
        let manifest_path = dir.join("manifest.json");
        fs::write(&manifest_path, &bytes).await?;
        set_file_permissions_unix(&manifest_path, 0o640).await?;

        Ok(MultipartUpload {
            key: key.to_string(),
            upload_id,
        })
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        data: Bytes,
    ) -> Result<CompletedPart> {
        let key = validate_storage_key(key)?;
        if !(1..=10_000).contains(&part_number) {
            return Err(StorageError::Backend(format!(
                "invalid part_number {part_number}: must be 1..=10000"
            )));
        }

        let dir = self.upload_dir(upload_id)?;
        let manifest_bytes = match fs::read(dir.join("manifest.json")).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(format!("upload_id {upload_id}")));
            }
            Err(e) => return Err(e.into()),
        };
        let manifest: MultipartManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| StorageError::Backend(format!("manifest parse error: {e}")))?;
        if manifest.key != key {
            return Err(StorageError::InvalidKey(format!(
                "upload_id {upload_id} does not match key {key}"
            )));
        }

        self.check_disk_space(data.len() as u64).await?;

        let etag = sha256_hex(&data);
        let tmp_part = self.tmp_dir().join(format!("{}.part", Uuid::new_v4()));
        fs::write(&tmp_part, &data).await?;
        set_file_permissions_unix(&tmp_part, 0o640).await?;
        let part_path = dir.join(format!("part-{part_number:06}.bin"));
        fs::rename(&tmp_part, &part_path).await?;

        Ok(CompletedPart { part_number, etag })
    }

    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[CompletedPart],
    ) -> Result<ObjectMeta> {
        let target_path = self.object_path(key)?;
        if parts.is_empty() {
            return Err(StorageError::Backend(
                "complete_multipart requires at least one part".to_string(),
            ));
        }

        let dir = self.upload_dir(upload_id)?;
        let manifest_bytes = match fs::read(dir.join("manifest.json")).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(format!("upload_id {upload_id}")));
            }
            Err(e) => return Err(e.into()),
        };
        let manifest: MultipartManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| StorageError::Backend(format!("manifest parse error: {e}")))?;
        if manifest.key != key {
            return Err(StorageError::InvalidKey(format!(
                "upload_id {upload_id} does not match key {key}"
            )));
        }

        let mut sorted_parts = parts.to_vec();
        sorted_parts.sort_by_key(|p| p.part_number);

        let mut total_expected: u64 = 0;
        for part in &sorted_parts {
            let part_path = dir.join(format!("part-{:06}.bin", part.part_number));
            let meta = fs::metadata(&part_path).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    StorageError::NotFound(format!(
                        "part {} for upload {upload_id}",
                        part.part_number
                    ))
                } else {
                    e.into()
                }
            })?;
            total_expected = total_expected.saturating_add(meta.len());
        }

        self.check_disk_space(total_expected).await?;
        self.verify_no_symlinks(&target_path).await?;
        if let Some(parent) = target_path.parent() {
            create_dir_all_secure(parent).await?;
        }

        let tmp_path = self.tmp_dir().join(format!("{}.part", Uuid::new_v4()));
        let out_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .await?;
        let mut writer = BufWriter::with_capacity(STREAM_BUF_SIZE, out_file);
        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buf = vec![0u8; STREAM_BUF_SIZE];
        let mut total_written: u64 = 0;

        let assemble_res: Result<()> = async {
            for part in &sorted_parts {
                let part_path = dir.join(format!("part-{:06}.bin", part.part_number));
                let mut part_file = File::open(&part_path).await?;
                loop {
                    let n = part_file.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                    writer.write_all(&buf[..n]).await?;
                    total_written = total_written.saturating_add(n as u64);
                }
            }
            writer.flush().await?;
            writer.get_ref().sync_all().await?;
            Ok(())
        }
        .await;

        if let Err(err) = assemble_res {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(err);
        }

        set_file_permissions_unix(&tmp_path, 0o640).await?;
        let etag = hex::encode(hasher.finish().as_ref());
        let meta = LocalObjectMetadata {
            content_type: manifest.content_type.clone(),
            etag: etag.clone(),
            size_bytes: total_written,
        };
        self.write_metadata(key, &meta).await?;
        fs::rename(&tmp_path, &target_path).await?;
        let _ = fs::remove_dir_all(&dir).await;

        Ok(ObjectMeta {
            size_bytes: total_written,
            content_type: Some(manifest.content_type),
            etag: Some(etag),
        })
    }

    async fn abort_multipart(&self, _key: &str, upload_id: &str) -> Result<()> {
        let dir = self.upload_dir(upload_id)?;
        match fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn health_check(&self) -> Result<()> {
        self.ensure_base_dirs().await?;
        self.check_disk_space(0).await?;
        Ok(())
    }
}

async fn create_dir_all_secure(path: &Path) -> Result<()> {
    fs::create_dir_all(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o750);
        let _ = fs::set_permissions(path, perms).await;
    }
    Ok(())
}

async fn set_file_permissions_unix(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perms).await?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
