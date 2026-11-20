//! S3 / RustFS object storage backend using `reqwest` and AWS SigV4 (`ring`).

use std::fmt;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use futures_util::TryStreamExt;
use reqwest::{Client, Method, Response, StatusCode, Url};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tracing::warn;

use crate::key::validate_storage_key;
use crate::sigv4::{SigV4Params, UNSIGNED_PAYLOAD, aws_uri_encode, sha256_hex, sign_request};
use crate::{
    ByteStream, CompletedPart, MultipartUpload, ObjectMeta, RangeObject, Result, StorageBackend,
    StorageError,
};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: usize = 3;
const BASE_BACKOFF_MS: u64 = 100;
const MAX_BACKOFF_MS: u64 = 2_000;
const STREAM_BUF_SIZE: usize = 64 * 1024;

/// Configuration for [`S3Storage`].
#[derive(Clone, PartialEq, Eq)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub force_path_style: bool,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key", &mask_key(&self.access_key))
            .field("secret_key", &"[REDACTED]")
            .field("force_path_style", &self.force_path_style)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .finish()
    }
}

impl S3Config {
    /// Loads S3 configuration from standard environment variables:
    /// `S3_ENDPOINT`, `S3_BUCKET`, `S3_REGION`, `S3_FORCE_PATH_STYLE`,
    /// `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`.
    pub fn from_env() -> Result<Self> {
        let endpoint = std::env::var("S3_ENDPOINT")
            .map_err(|_| StorageError::Backend("missing S3_ENDPOINT".to_string()))?;
        let bucket = std::env::var("S3_BUCKET")
            .map_err(|_| StorageError::Backend("missing S3_BUCKET".to_string()))?;
        let region = std::env::var("S3_REGION")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "us-east-1".to_string());
        let access_key = std::env::var("S3_ACCESS_KEY_ID").unwrap_or_default();
        let secret_key = std::env::var("S3_SECRET_ACCESS_KEY").unwrap_or_default();
        let force_path_style = std::env::var("S3_FORCE_PATH_STYLE")
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(true);

        Ok(Self {
            endpoint,
            bucket,
            region,
            access_key,
            secret_key,
            force_path_style,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
        })
    }

    /// Validates that endpoint URL and bucket are well-formed.
    pub fn validate(&self) -> Result<Url> {
        if self.bucket.trim().is_empty() {
            return Err(StorageError::Backend(
                "S3 bucket cannot be empty".to_string(),
            ));
        }
        let url = Url::parse(self.endpoint.trim())
            .map_err(|e| StorageError::Backend(format!("invalid S3 endpoint URL: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(StorageError::Backend(format!(
                "unsupported S3 endpoint scheme: {}",
                url.scheme()
            )));
        }
        if url.host_str().is_none() {
            return Err(StorageError::Backend(
                "S3 endpoint URL missing host".to_string(),
            ));
        }
        Ok(url)
    }
}

/// S3 / RustFS implementation of [`StorageBackend`].
#[derive(Clone)]
pub struct S3Storage {
    config: S3Config,
    endpoint_url: Url,
    client: Client,
}

impl fmt::Debug for S3Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Storage")
            .field("config", &self.config)
            .finish()
    }
}

impl S3Storage {
    /// Builds a new [`S3Storage`] client with strict TLS validation and timeouts.
    pub fn new(config: S3Config) -> Result<Self> {
        let endpoint_url = config.validate()?;
        let client = Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .danger_accept_invalid_certs(false)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            config,
            endpoint_url,
            client,
        })
    }

    /// Returns a reference to the sanitized configuration.
    pub fn config(&self) -> &S3Config {
        &self.config
    }

    fn build_target(&self, key: Option<&str>) -> Result<(Url, String, String)> {
        let mut url = self.endpoint_url.clone();
        let base_host = url
            .host_str()
            .ok_or_else(|| StorageError::Backend("missing endpoint host".to_string()))?
            .to_string();

        let base_path = url.path().trim_end_matches('/').to_string();
        let key_part = match key {
            Some(k) => {
                let valid = validate_storage_key(k)?;
                Some(valid)
            }
            None => None,
        };

        let (host_header, raw_path) = if self.config.force_path_style {
            let host_hdr = match url.port() {
                Some(port) => format!("{base_host}:{port}"),
                None => base_host,
            };
            let path = match key_part {
                Some(k) => format!("{base_path}/{}/{k}", self.config.bucket),
                None => format!("{base_path}/{}", self.config.bucket),
            };
            (host_hdr, path)
        } else {
            let vhost = format!("{}.{base_host}", self.config.bucket);
            url.set_host(Some(&vhost))
                .map_err(|e| StorageError::Backend(format!("invalid bucket host: {e}")))?;
            let host_hdr = match url.port() {
                Some(port) => format!("{vhost}:{port}"),
                None => vhost,
            };
            let path = match key_part {
                Some(k) => format!("{base_path}/{k}"),
                None => {
                    if base_path.is_empty() {
                        "/".to_string()
                    } else {
                        base_path.to_string()
                    }
                }
            };
            (host_hdr, path)
        };

        let normalized_path = if raw_path.starts_with('/') {
            raw_path
        } else {
            format!("/{raw_path}")
        };
        url.set_path(&aws_uri_encode(&normalized_path, false));
        Ok((url, host_header, normalized_path))
    }

    fn build_signed_request(
        &self,
        method: Method,
        key: Option<&str>,
        query: &[(&str, &str)],
        extra_headers: &[(&str, &str)],
        payload_hash: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let (mut url, host_header, canonical_path) = self.build_target(key)?;
        if !query.is_empty() {
            let qs = query
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        aws_uri_encode(k, true)
                    } else {
                        format!("{}={}", aws_uri_encode(k, true), aws_uri_encode(v, true))
                    }
                })
                .collect::<Vec<_>>()
                .join("&");
            url.set_query(Some(&qs));
        }

        let sig = sign_request(&SigV4Params {
            method: method.as_str(),
            host: &host_header,
            path: &canonical_path,
            query,
            headers: extra_headers,
            payload_hash,
            access_key: &self.config.access_key,
            secret_key: &self.config.secret_key,
            region: &self.config.region,
            now: Utc::now(),
        });

        let mut req = self
            .client
            .request(method, url)
            .header("host", &host_header)
            .header("x-amz-date", &sig.x_amz_date)
            .header("x-amz-content-sha256", &sig.x_amz_content_sha256)
            .header("authorization", &sig.authorization);

        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }

        Ok(req)
    }

    async fn send_with_retry<F>(&self, mut make_req: F) -> Result<Response>
    where
        F: FnMut() -> Result<reqwest::RequestBuilder>,
    {
        let mut attempt = 0usize;
        loop {
            let req = make_req()?;
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if is_retryable_status(status) && attempt < MAX_RETRIES {
                        let delay = backoff_duration(attempt);
                        warn!(
                            status = %status,
                            attempt = attempt + 1,
                            delay_ms = delay.as_millis(),
                            "transient S3 HTTP error; retrying"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    if is_retryable_status(status) {
                        return Err(StorageError::Unavailable(format!(
                            "S3 service unavailable after {attempt} retries (HTTP {status})"
                        )));
                    }
                    return Ok(resp);
                }
                Err(err) => {
                    if is_retryable_error(&err) && attempt < MAX_RETRIES {
                        let delay = backoff_duration(attempt);
                        warn!(
                            error = %err,
                            attempt = attempt + 1,
                            delay_ms = delay.as_millis(),
                            "transient S3 transport error; retrying"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    if err.is_connect() || err.is_timeout() {
                        return Err(StorageError::Unavailable(format!(
                            "S3 endpoint unreachable: {err}"
                        )));
                    }
                    return Err(StorageError::Http(err));
                }
            }
        }
    }

    /// Checks bucket accessibility (`HEAD /{bucket}` or `/`).
    pub async fn check_bucket(&self) -> Result<()> {
        let resp = self
            .send_with_retry(|| {
                self.build_signed_request(Method::HEAD, None, &[], &[], &sha256_hex(&[]))
            })
            .await?;

        match resp.status() {
            s if s.is_success() => Ok(()),
            StatusCode::NOT_FOUND => Err(StorageError::NotFound(format!(
                "bucket {}",
                self.config.bucket
            ))),
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
                Err(StorageError::Backend(format!(
                    "S3 authentication/permission denied (HTTP {})",
                    resp.status()
                )))
            }
            other => Err(StorageError::Backend(format!(
                "S3 bucket check failed (HTTP {other})"
            ))),
        }
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn put_object(
        &self,
        key: &str,
        stream: ByteStream,
        size_bytes: Option<u64>,
        content_type: &str,
    ) -> Result<ObjectMeta> {
        let key = validate_storage_key(key)?;
        let len_str = size_bytes.map(|s| s.to_string());
        let mut headers = vec![("content-type", content_type)];
        if let Some(ref ls) = len_str {
            headers.push(("content-length", ls.as_str()));
        }

        let req = self
            .build_signed_request(Method::PUT, Some(key), &[], &headers, UNSIGNED_PAYLOAD)?
            .body(reqwest::Body::wrap_stream(stream));

        let resp = req.send().await.map_err(|err| {
            if err.is_connect() || err.is_timeout() {
                StorageError::Unavailable(format!("S3 endpoint unreachable: {err}"))
            } else {
                StorageError::Http(err)
            }
        })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp, key).await);
        }

        let etag = extract_etag(resp.headers());
        Ok(ObjectMeta {
            size_bytes: size_bytes.unwrap_or(0),
            content_type: Some(content_type.to_string()),
            etag,
        })
    }

    async fn put_bytes(&self, key: &str, data: Bytes, content_type: &str) -> Result<ObjectMeta> {
        let key = validate_storage_key(key)?;
        let payload_hash = sha256_hex(&data);
        let size = data.len() as u64;
        let len_str = size.to_string();

        let resp = self
            .send_with_retry(|| {
                let headers = [
                    ("content-type", content_type),
                    ("content-length", len_str.as_str()),
                ];
                Ok(self
                    .build_signed_request(Method::PUT, Some(key), &[], &headers, &payload_hash)?
                    .body(data.clone()))
            })
            .await?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp, key).await);
        }

        let etag = extract_etag(resp.headers());
        Ok(ObjectMeta {
            size_bytes: size,
            content_type: Some(content_type.to_string()),
            etag,
        })
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
        let key = validate_storage_key(key)?;
        let range_hdr = match end_inclusive {
            Some(end) => {
                if end < start {
                    return Err(StorageError::InvalidRange(format!(
                        "range end {end} < start {start}"
                    )));
                }
                format!("bytes={start}-{end}")
            }
            None => format!("bytes={start}-"),
        };
        let empty_hash = sha256_hex(&[]);

        let resp = self
            .send_with_retry(|| {
                let headers = [("range", range_hdr.as_str())];
                self.build_signed_request(Method::GET, Some(key), &[], &headers, &empty_hash)
            })
            .await?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(key.to_string()));
        }
        if status == StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(StorageError::InvalidRange(range_hdr));
        }
        if !status.is_success() {
            return Err(map_error_response(resp, key).await);
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        let etag = extract_etag(resp.headers());
        let content_length = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        let (resp_start, resp_end, total_size) = if status == StatusCode::PARTIAL_CONTENT {
            if let Some(cr) = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
            {
                cr
            } else {
                let len = content_length.unwrap_or(0);
                let end =
                    end_inclusive.unwrap_or_else(|| start.saturating_add(len.saturating_sub(1)));
                (start, end, end.saturating_add(1))
            }
        } else {
            let total = content_length.unwrap_or(0);
            let end = total.saturating_sub(1);
            (0, end, total)
        };

        let stream: ByteStream = Box::pin(
            resp.bytes_stream()
                .map_err(|e| std::io::Error::other(e.to_string())),
        );

        Ok(RangeObject {
            stream,
            total_size,
            start: resp_start,
            end_inclusive: resp_end,
            content_type,
            etag,
        })
    }

    async fn head_object(&self, key: &str) -> Result<ObjectMeta> {
        let key = validate_storage_key(key)?;
        let empty_hash = sha256_hex(&[]);
        let resp = self
            .send_with_retry(|| {
                self.build_signed_request(Method::HEAD, Some(key), &[], &[], &empty_hash)
            })
            .await?;

        if resp.status() == StatusCode::NOT_FOUND {
            return Err(StorageError::NotFound(key.to_string()));
        }
        if !resp.status().is_success() {
            return Err(map_error_response(resp, key).await);
        }

        let size_bytes = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(ToString::to_string);
        let etag = extract_etag(resp.headers());

        Ok(ObjectMeta {
            size_bytes,
            content_type,
            etag,
        })
    }

    async fn delete_object(&self, key: &str) -> Result<()> {
        let key = validate_storage_key(key)?;
        let empty_hash = sha256_hex(&[]);
        let resp = self
            .send_with_retry(|| {
                self.build_signed_request(Method::DELETE, Some(key), &[], &[], &empty_hash)
            })
            .await?;

        if resp.status() == StatusCode::NOT_FOUND || resp.status().is_success() {
            return Ok(());
        }
        Err(map_error_response(resp, key).await)
    }

    async fn create_multipart(&self, key: &str, content_type: &str) -> Result<MultipartUpload> {
        let key = validate_storage_key(key)?;
        let empty_hash = sha256_hex(&[]);
        let resp = self
            .send_with_retry(|| {
                let headers = [("content-type", content_type)];
                self.build_signed_request(
                    Method::POST,
                    Some(key),
                    &[("uploads", "")],
                    &headers,
                    &empty_hash,
                )
            })
            .await?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp, key).await);
        }

        let body = resp.text().await?;
        let upload_id = extract_xml_tag(&body, "UploadId").ok_or_else(|| {
            StorageError::Backend("S3 CreateMultipartUpload response missing UploadId".to_string())
        })?;

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
        let part_str = part_number.to_string();
        let payload_hash = sha256_hex(&data);
        let len_str = data.len().to_string();

        let resp = self
            .send_with_retry(|| {
                let query = [("partNumber", part_str.as_str()), ("uploadId", upload_id)];
                let headers = [("content-length", len_str.as_str())];
                Ok(self
                    .build_signed_request(Method::PUT, Some(key), &query, &headers, &payload_hash)?
                    .body(data.clone()))
            })
            .await?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp, key).await);
        }

        let etag = extract_etag(resp.headers()).ok_or_else(|| {
            StorageError::Backend(format!("S3 UploadPart {part_number} missing ETag header"))
        })?;

        Ok(CompletedPart { part_number, etag })
    }

    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[CompletedPart],
    ) -> Result<ObjectMeta> {
        let key = validate_storage_key(key)?;
        if parts.is_empty() {
            return Err(StorageError::Backend(
                "complete_multipart requires at least one part".to_string(),
            ));
        }

        let mut sorted = parts.to_vec();
        sorted.sort_by_key(|p| p.part_number);

        let mut xml = String::from("<CompleteMultipartUpload>");
        for part in &sorted {
            xml.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                part.part_number,
                escape_xml(&part.etag)
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");

        let body_bytes = Bytes::from(xml);
        let payload_hash = sha256_hex(&body_bytes);
        let len_str = body_bytes.len().to_string();

        let resp = self
            .send_with_retry(|| {
                let query = [("uploadId", upload_id)];
                let headers = [
                    ("content-type", "application/xml"),
                    ("content-length", len_str.as_str()),
                ];
                Ok(self
                    .build_signed_request(Method::POST, Some(key), &query, &headers, &payload_hash)?
                    .body(body_bytes.clone()))
            })
            .await?;

        let status = resp.status();
        let header_etag = extract_etag(resp.headers());
        let text = resp.text().await?;
        if !status.is_success() || text.contains("<Error>") {
            let code = extract_xml_tag(&text, "Code").unwrap_or_else(|| status.to_string());
            let msg = extract_xml_tag(&text, "Message").unwrap_or(text);
            return Err(StorageError::Backend(format!(
                "S3 CompleteMultipartUpload failed ({code}): {msg}"
            )));
        }

        let xml_etag = extract_xml_tag(&text, "ETag").map(|s| s.trim_matches('"').to_string());
        let head = self.head_object(key).await.ok();

        Ok(ObjectMeta {
            size_bytes: head.as_ref().map(|h| h.size_bytes).unwrap_or(0),
            content_type: head.as_ref().and_then(|h| h.content_type.clone()),
            etag: head.and_then(|h| h.etag).or(xml_etag).or(header_etag),
        })
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<()> {
        let key = validate_storage_key(key)?;
        let empty_hash = sha256_hex(&[]);
        let resp = self
            .send_with_retry(|| {
                self.build_signed_request(
                    Method::DELETE,
                    Some(key),
                    &[("uploadId", upload_id)],
                    &[],
                    &empty_hash,
                )
            })
            .await?;

        if resp.status() == StatusCode::NOT_FOUND || resp.status().is_success() {
            return Ok(());
        }
        Err(map_error_response(resp, key).await)
    }

    async fn health_check(&self) -> Result<()> {
        self.check_bucket().await
    }
}

fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    if key.len() <= 4 {
        return "***".to_string();
    }
    format!("{}***", &key[..4])
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn is_retryable_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_request()
}

fn backoff_duration(attempt: usize) -> Duration {
    let factor = 1u64.checked_shl(attempt as u32).unwrap_or(16);
    let ms = BASE_BACKOFF_MS.saturating_mul(factor).min(MAX_BACKOFF_MS);
    Duration::from_millis(ms)
}

fn extract_etag(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string())
}

fn parse_content_range(header: &str) -> Option<(u64, u64, u64)> {
    // Format: "bytes start-end/total"
    let rest = header.strip_prefix("bytes ")?.trim();
    let (range_part, total_part) = rest.split_once('/')?;
    let total: u64 = total_part.parse().ok()?;
    let (start_s, end_s) = range_part.split_once('-')?;
    let start: u64 = start_s.parse().ok()?;
    let end: u64 = end_s.parse().ok()?;
    Some((start, end, total))
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

async fn map_error_response(resp: Response, key: &str) -> StorageError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let code = extract_xml_tag(&body, "Code");
    let msg = extract_xml_tag(&body, "Message");
    match status {
        StatusCode::NOT_FOUND => StorageError::NotFound(key.to_string()),
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT => {
            StorageError::Unavailable(format!("S3 HTTP {status}"))
        }
        _ => StorageError::Backend(format!(
            "S3 HTTP {status} for {key}: {}",
            msg.or(code).unwrap_or_else(|| "request failed".to_string())
        )),
    }
}
