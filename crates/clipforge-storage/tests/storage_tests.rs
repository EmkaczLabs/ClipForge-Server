use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use clipforge_storage::{
    ByteStream, CapabilityStatus, CheckStatus, ClipObjectKind, LocalStorage, S3Config, S3Storage,
    StorageBackend, StorageCheckOptions, StorageConfig, StorageError, build_clip_object_key,
    run_storage_check, validate_id_segment, validate_storage_key,
};
use futures_util::StreamExt;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

async fn collect_stream(mut stream: ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("stream chunk"));
    }
    out
}

#[test]
fn key_generation_and_path_traversal_validation() {
    let orig = build_clip_object_key("user_123", "clip-abc", ClipObjectKind::Original).unwrap();
    assert_eq!(orig, "clips/user_123/clip-abc/original.mp4");
    assert_eq!(
        build_clip_object_key("user_123", "clip-abc", ClipObjectKind::Thumbnail).unwrap(),
        "clips/user_123/clip-abc/thumbnail.jpg"
    );
    assert_eq!(
        build_clip_object_key("user_123", "clip-abc", ClipObjectKind::Preview).unwrap(),
        "clips/user_123/clip-abc/preview.mp4"
    );

    assert!(validate_id_segment("valid-ID_01").is_ok());
    assert!(validate_id_segment("").is_err());
    assert!(validate_id_segment("../etc").is_err());
    assert!(validate_id_segment("a/b").is_err());
    assert!(validate_id_segment("a\\b").is_err());
    assert!(validate_id_segment("a.b").is_err());
    assert!(validate_id_segment("%2e%2e").is_err());

    assert!(validate_storage_key("clips/u1/c1/original.mp4").is_ok());
    for bad in [
        "",
        "/leading/slash",
        "trailing/slash/",
        "double//slash",
        "back\\slash",
        "null\0byte",
        "../escape",
        "clips/../etc/passwd",
        ".",
        "clips/./a",
        ".tmp/secret",
        ".meta/secret",
        ".multipart/secret",
    ] {
        assert!(
            matches!(validate_storage_key(bad), Err(StorageError::InvalidKey(_))),
            "expected InvalidKey for {bad:?}"
        );
    }
}

#[tokio::test]
async fn local_storage_put_head_range_delete_and_content_type() {
    let dir = TempDir::new().unwrap();
    let store = LocalStorage::new(dir.path());

    let payload = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let key = build_clip_object_key("u1", "c1", ClipObjectKind::Original).unwrap();

    // Streamed put_object with multiple chunks
    let chunks: Vec<std::result::Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from_static(&payload[..10])),
        Ok(Bytes::from_static(&payload[10..20])),
        Ok(Bytes::from_static(&payload[20..])),
    ];
    let stream: ByteStream = Box::pin(futures_util::stream::iter(chunks));
    let meta = store
        .put_object(&key, stream, Some(payload.len() as u64), "video/mp4")
        .await
        .unwrap();
    assert_eq!(meta.size_bytes, payload.len() as u64);
    assert_eq!(meta.content_type.as_deref(), Some("video/mp4"));
    assert!(meta.etag.is_some());

    let head = store.head_object(&key).await.unwrap();
    assert_eq!(head.size_bytes, payload.len() as u64);
    assert_eq!(head.content_type.as_deref(), Some("video/mp4"));

    // Middle range
    let range = store.get_object_range(&key, 5, Some(14)).await.unwrap();
    assert_eq!(range.start, 5);
    assert_eq!(range.end_inclusive, 14);
    assert_eq!(range.total_size, payload.len() as u64);
    assert_eq!(range.content_type.as_deref(), Some("video/mp4"));
    let got = collect_stream(range.stream).await;
    assert_eq!(&got, &payload[5..=14]);

    // Open-ended range
    let tail = store.get_object_range(&key, 26, None).await.unwrap();
    assert_eq!(tail.end_inclusive, payload.len() as u64 - 1);
    assert_eq!(&collect_stream(tail.stream).await, &payload[26..]);

    // Invalid range
    assert!(matches!(
        store.get_object_range(&key, 999, None).await,
        Err(StorageError::InvalidRange(_))
    ));

    // Delete is idempotent
    store.delete_object(&key).await.unwrap();
    store.delete_object(&key).await.unwrap();
    assert!(matches!(
        store.head_object(&key).await,
        Err(StorageError::NotFound(_))
    ));
}

#[tokio::test]
async fn local_storage_multipart_complete_abort_and_cleanup() {
    let dir = TempDir::new().unwrap();
    let store = LocalStorage::new(dir.path());
    let key = build_clip_object_key("u2", "c2", ClipObjectKind::Preview).unwrap();

    // Complete multipart (uploading parts out of order)
    let upload = store.create_multipart(&key, "video/mp4").await.unwrap();
    let p2 = store
        .upload_part(&key, &upload.upload_id, 2, Bytes::from_static(b"world!"))
        .await
        .unwrap();
    let p1 = store
        .upload_part(&key, &upload.upload_id, 1, Bytes::from_static(b"hello "))
        .await
        .unwrap();
    let meta = store
        .complete_multipart(&key, &upload.upload_id, &[p2, p1])
        .await
        .unwrap();
    assert_eq!(meta.size_bytes, 12);
    assert_eq!(meta.content_type.as_deref(), Some("video/mp4"));

    let obj = store.get_object_range(&key, 0, None).await.unwrap();
    assert_eq!(collect_stream(obj.stream).await, b"hello world!");

    // Abort multipart cleans staging directory
    let key2 = build_clip_object_key("u2", "c3", ClipObjectKind::Thumbnail).unwrap();
    let upload2 = store.create_multipart(&key2, "image/webp").await.unwrap();
    store
        .upload_part(&key2, &upload2.upload_id, 1, Bytes::from_static(b"part1"))
        .await
        .unwrap();
    store
        .abort_multipart(&key2, &upload2.upload_id)
        .await
        .unwrap();
    assert!(
        !dir.path()
            .join(".multipart")
            .join(&upload2.upload_id)
            .exists()
    );

    // Cleanup incomplete uploads with zero max_age removes stale multipart
    let upload3 = store.create_multipart(&key2, "image/webp").await.unwrap();
    let removed = store
        .cleanup_incomplete_uploads(Duration::from_secs(0))
        .await
        .unwrap();
    assert!(removed >= 1);
    assert!(
        !dir.path()
            .join(".multipart")
            .join(&upload3.upload_id)
            .exists()
    );
}

#[tokio::test]
async fn local_storage_disk_space_guard() {
    let dir = TempDir::new().unwrap();
    let store = LocalStorage::new(dir.path()).with_min_free_bytes(u64::MAX / 2);
    let key = build_clip_object_key("u3", "c3", ClipObjectKind::Original).unwrap();

    let res = store
        .put_bytes(&key, Bytes::from_static(b"abc"), "video/mp4")
        .await;
    assert!(matches!(res, Err(StorageError::InsufficientSpace { .. })));
}

#[tokio::test]
async fn storage_check_local_readonly_vs_opt_in_probe() {
    let dir = TempDir::new().unwrap();
    let cfg = StorageConfig::Local {
        root_dir: dir.path().to_path_buf(),
    };

    // Read-only by default
    let report = run_storage_check(&cfg, &StorageCheckOptions::default()).await;
    assert!(report.healthy);
    assert_eq!(report.backend, "local");
    assert_eq!(
        report.capabilities.write,
        CapabilityStatus::SupportedUnverified
    );
    assert!(
        report
            .checks
            .iter()
            .any(|c| c.name == "write_probe" && c.status == CheckStatus::Skipped)
    );
    assert!(!dir.path().join("_clipforge_probe").exists());

    // Opt-in write probe
    let probe_opts = StorageCheckOptions {
        allow_write_probe: true,
        probe_prefix: "_clipforge_probe".to_string(),
    };
    let report2 = run_storage_check(&cfg, &probe_opts).await;
    assert!(report2.healthy);
    assert_eq!(report2.capabilities.write, CapabilityStatus::Verified);
    assert_eq!(report2.capabilities.range_read, CapabilityStatus::Verified);
    assert_eq!(report2.capabilities.delete, CapabilityStatus::Verified);
    assert!(
        report2
            .checks
            .iter()
            .any(|c| c.name == "write_probe" && c.status == CheckStatus::Pass)
    );
}

// --- Minimal Mock S3 HTTP Server for S3Storage tests ---

type MockMultipartEntry = (String, String, HashMap<u32, (Vec<u8>, String)>);

#[derive(Default)]
struct MockS3State {
    objects: HashMap<String, (Vec<u8>, String, String)>, // key -> (data, content_type, etag)
    multiparts: HashMap<String, MockMultipartEntry>,     // upload_id -> (key, ct, parts)
    fail_next_requests: AtomicUsize,
    seen_auth_headers: Vec<String>,
    put_count: usize,
}

async fn start_mock_s3() -> (String, Arc<Mutex<MockS3State>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(Mutex::new(MockS3State::default()));
    let state_clone = state.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let st = state_clone.clone();
            tokio::spawn(async move {
                loop {
                    let Some((method, target, headers, body)) =
                        read_http_request(&mut socket).await
                    else {
                        break;
                    };

                    let mut guard = st.lock().await;
                    if let Some(auth) = headers.get("authorization") {
                        guard.seen_auth_headers.push(auth.clone());
                    }

                    let remaining_fails = guard.fail_next_requests.load(Ordering::SeqCst);
                    if remaining_fails > 0 {
                        guard.fail_next_requests.fetch_sub(1, Ordering::SeqCst);
                        drop(guard);
                        let _ = write_http_response(
                            &mut socket,
                            503,
                            "Service Unavailable",
                            &[("Content-Length", "0")],
                            b"",
                        )
                        .await;
                        continue;
                    }

                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    // Path-style: /{bucket}/{key...}
                    let trimmed = path.trim_start_matches('/');
                    let key = trimmed.split_once('/').map(|(_, k)| k).unwrap_or("");

                    let (status, reason, resp_headers, resp_body) = match method.as_str() {
                        "HEAD" => {
                            if key.is_empty() {
                                (200, "OK", vec![], vec![])
                            } else if let Some((data, ct, etag)) = guard.objects.get(key) {
                                (
                                    200,
                                    "OK",
                                    vec![
                                        ("Content-Type".to_string(), ct.clone()),
                                        ("Content-Length".to_string(), data.len().to_string()),
                                        ("ETag".to_string(), format!("\"{etag}\"")),
                                    ],
                                    vec![],
                                )
                            } else {
                                (404, "Not Found", vec![], vec![])
                            }
                        }
                        "PUT" => {
                            if query.contains("uploadId=") && query.contains("partNumber=") {
                                let part_num: u32 = query_param(query, "partNumber")
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(1);
                                let upload_id = query_param(query, "uploadId").unwrap_or_default();
                                let etag = format!("part-etag-{part_num}");
                                if let Some((_, _, parts)) = guard.multiparts.get_mut(&upload_id) {
                                    parts.insert(part_num, (body, etag.clone()));
                                    (
                                        200,
                                        "OK",
                                        vec![("ETag".to_string(), format!("\"{etag}\""))],
                                        vec![],
                                    )
                                } else {
                                    (404, "Not Found", vec![], vec![])
                                }
                            } else {
                                let ct = headers
                                    .get("content-type")
                                    .cloned()
                                    .unwrap_or_else(|| "application/octet-stream".to_string());
                                let etag = format!("etag-{}", body.len());
                                guard.put_count += 1;
                                guard
                                    .objects
                                    .insert(key.to_string(), (body, ct, etag.clone()));
                                (
                                    200,
                                    "OK",
                                    vec![("ETag".to_string(), format!("\"{etag}\""))],
                                    vec![],
                                )
                            }
                        }
                        "GET" => {
                            if let Some((data, ct, etag)) = guard.objects.get(key) {
                                if let Some(range_hdr) = headers.get("range") {
                                    if let Some((start, end_opt)) = parse_req_range(range_hdr) {
                                        if start >= data.len() as u64 {
                                            (416, "Range Not Satisfiable", vec![], vec![])
                                        } else {
                                            let end = end_opt
                                                .unwrap_or(data.len() as u64 - 1)
                                                .min(data.len() as u64 - 1);
                                            let slice =
                                                data[start as usize..=end as usize].to_vec();
                                            (
                                                206,
                                                "Partial Content",
                                                vec![
                                                    ("Content-Type".to_string(), ct.clone()),
                                                    (
                                                        "Content-Range".to_string(),
                                                        format!(
                                                            "bytes {start}-{end}/{}",
                                                            data.len()
                                                        ),
                                                    ),
                                                    (
                                                        "Content-Length".to_string(),
                                                        slice.len().to_string(),
                                                    ),
                                                    ("ETag".to_string(), format!("\"{etag}\"")),
                                                ],
                                                slice,
                                            )
                                        }
                                    } else {
                                        (400, "Bad Request", vec![], vec![])
                                    }
                                } else {
                                    (
                                        200,
                                        "OK",
                                        vec![
                                            ("Content-Type".to_string(), ct.clone()),
                                            ("Content-Length".to_string(), data.len().to_string()),
                                            ("ETag".to_string(), format!("\"{etag}\"")),
                                        ],
                                        data.clone(),
                                    )
                                }
                            } else {
                                (404, "Not Found", vec![], vec![])
                            }
                        }
                        "POST" => {
                            if query == "uploads" || query.starts_with("uploads=") {
                                let upload_id = format!("up-{}", guard.multiparts.len() + 1);
                                let ct = headers
                                    .get("content-type")
                                    .cloned()
                                    .unwrap_or_else(|| "video/mp4".to_string());
                                guard.multiparts.insert(
                                    upload_id.clone(),
                                    (key.to_string(), ct, HashMap::new()),
                                );
                                let xml = format!(
                                    "<InitiateMultipartUploadResult><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
                                );
                                (200, "OK", vec![], xml.into_bytes())
                            } else if let Some(upload_id) = query_param(query, "uploadId") {
                                if let Some((ukey, ct, parts)) = guard.multiparts.remove(&upload_id)
                                {
                                    let mut keys: Vec<u32> = parts.keys().copied().collect();
                                    keys.sort();
                                    let mut assembled = Vec::new();
                                    for k in keys {
                                        assembled.extend_from_slice(&parts[&k].0);
                                    }
                                    let etag = format!("mp-etag-{}", assembled.len());
                                    guard.objects.insert(ukey, (assembled, ct, etag.clone()));
                                    let xml = format!(
                                        "<CompleteMultipartUploadResult><ETag>\"{etag}\"</ETag></CompleteMultipartUploadResult>"
                                    );
                                    (200, "OK", vec![], xml.into_bytes())
                                } else {
                                    (404, "Not Found", vec![], vec![])
                                }
                            } else {
                                (400, "Bad Request", vec![], vec![])
                            }
                        }
                        "DELETE" => {
                            if let Some(upload_id) = query_param(query, "uploadId") {
                                if guard.multiparts.remove(&upload_id).is_some() {
                                    (204, "No Content", vec![], vec![])
                                } else {
                                    (404, "Not Found", vec![], vec![])
                                }
                            } else if guard.objects.remove(key).is_some() {
                                (204, "No Content", vec![], vec![])
                            } else {
                                (404, "Not Found", vec![], vec![])
                            }
                        }
                        _ => (405, "Method Not Allowed", vec![], vec![]),
                    };

                    drop(guard);
                    let mut hdrs = resp_headers;
                    if !hdrs
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
                    {
                        hdrs.push(("Content-Length".to_string(), resp_body.len().to_string()));
                    }
                    let hdr_refs: Vec<(&str, &str)> =
                        hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    if write_http_response(&mut socket, status, reason, &hdr_refs, &resp_body)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
    });

    (format!("http://{addr}"), state)
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == name { Some(v.to_string()) } else { None }
    })
}

fn parse_req_range(hdr: &str) -> Option<(u64, Option<u64>)> {
    let rest = hdr.strip_prefix("bytes=")?;
    let (s, e) = rest.split_once('-')?;
    let start = s.parse().ok()?;
    let end = if e.is_empty() {
        None
    } else {
        Some(e.parse().ok()?)
    };
    Some((start, end))
}

async fn read_http_request(
    socket: &mut tokio::net::TcpStream,
) -> Option<(String, String, HashMap<String, String>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }

    let header_str = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let mut body = buf[header_end..].to_vec();
    if headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        // Decode chunked transfer encoding
        loop {
            if let Some(decoded) = try_decode_chunked(&body) {
                body = decoded;
                break;
            }
            let n = socket.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
    } else if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        while body.len() < len {
            let n = socket.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(len);
    }

    Some((method, target, headers, body))
}

fn try_decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let rel_end = raw[pos..].windows(2).position(|w| w == b"\r\n")?;
        let size_line = std::str::from_utf8(&raw[pos..pos + rel_end]).ok()?;
        let size = usize::from_str_radix(size_line.trim(), 16).ok()?;
        pos += rel_end + 2;
        if size == 0 {
            if raw.len() >= pos + 2 {
                return Some(out);
            }
            return None;
        }
        if raw.len() < pos + size + 2 {
            return None;
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2;
    }
}

async fn write_http_response(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await?;
    if !body.is_empty() {
        socket.write_all(body).await?;
    }
    socket.flush().await
}

fn test_s3_config(endpoint: String) -> S3Config {
    S3Config {
        endpoint,
        bucket: "clipforge-test".to_string(),
        region: "us-east-1".to_string(),
        access_key: "test-access-key".to_string(),
        secret_key: "super-secret-key-must-never-leak".to_string(),
        force_path_style: true,
        connect_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn s3_storage_put_range_head_delete_multipart_and_redacted_debug() {
    let (endpoint, state) = start_mock_s3().await;
    let cfg = test_s3_config(endpoint.clone());
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains("super-secret-key-must-never-leak"));
    assert!(dbg.contains("[REDACTED]"));

    let s3 = S3Storage::new(cfg).unwrap();
    let key = build_clip_object_key("u10", "c10", ClipObjectKind::Original).unwrap();

    // Put bytes + Head + Range
    let data = Bytes::from_static(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ");
    let meta = s3.put_bytes(&key, data.clone(), "video/mp4").await.unwrap();
    assert_eq!(meta.size_bytes, 26);
    assert_eq!(meta.content_type.as_deref(), Some("video/mp4"));

    let head = s3.head_object(&key).await.unwrap();
    assert_eq!(head.size_bytes, 26);
    assert_eq!(head.content_type.as_deref(), Some("video/mp4"));

    let range = s3.get_object_range(&key, 2, Some(7)).await.unwrap();
    assert_eq!(range.start, 2);
    assert_eq!(range.end_inclusive, 7);
    assert_eq!(range.total_size, 26);
    assert_eq!(collect_stream(range.stream).await, b"CDEFGH");

    // 416 Range Not Satisfiable
    assert!(matches!(
        s3.get_object_range(&key, 500, None).await,
        Err(StorageError::InvalidRange(_))
    ));

    // Delete + idempotent delete
    s3.delete_object(&key).await.unwrap();
    s3.delete_object(&key).await.unwrap();
    assert!(matches!(
        s3.head_object(&key).await,
        Err(StorageError::NotFound(_))
    ));

    // Multipart upload & complete
    let mp_key = build_clip_object_key("u10", "c11", ClipObjectKind::Preview).unwrap();
    let up = s3.create_multipart(&mp_key, "video/mp4").await.unwrap();
    let p1 = s3
        .upload_part(&mp_key, &up.upload_id, 1, Bytes::from_static(b"part1-"))
        .await
        .unwrap();
    let p2 = s3
        .upload_part(&mp_key, &up.upload_id, 2, Bytes::from_static(b"part2"))
        .await
        .unwrap();
    let mp_meta = s3
        .complete_multipart(&mp_key, &up.upload_id, &[p1, p2])
        .await
        .unwrap();
    assert_eq!(mp_meta.size_bytes, 11);
    let mp_obj = s3.get_object_range(&mp_key, 0, None).await.unwrap();
    assert_eq!(collect_stream(mp_obj.stream).await, b"part1-part2");

    // Multipart abort
    let up2 = s3.create_multipart(&mp_key, "video/mp4").await.unwrap();
    s3.abort_multipart(&mp_key, &up2.upload_id).await.unwrap();

    // Verify SigV4 Authorization header was sent
    let guard = state.lock().await;
    assert!(!guard.seen_auth_headers.is_empty());
    assert!(
        guard
            .seen_auth_headers
            .iter()
            .all(|h| h.starts_with("AWS4-HMAC-SHA256 Credential=test-access-key/"))
    );
}

#[tokio::test]
async fn s3_storage_retries_and_unavailability() {
    let (endpoint, state) = start_mock_s3().await;
    // Fail next 2 requests with 503, then succeed on 3rd attempt
    state
        .lock()
        .await
        .fail_next_requests
        .store(2, Ordering::SeqCst);

    let s3 = S3Storage::new(test_s3_config(endpoint)).unwrap();
    s3.health_check().await.unwrap();

    // Closed port -> Unavailable
    let bad_s3 = S3Storage::new(test_s3_config("http://127.0.0.1:1".to_string())).unwrap();
    let err = bad_s3.health_check().await.unwrap_err();
    assert!(matches!(err, StorageError::Unavailable(_)));
}

#[tokio::test]
async fn storage_check_s3_readonly_vs_opt_in_probe() {
    let (endpoint, state) = start_mock_s3().await;
    let cfg = StorageConfig::S3 {
        endpoint,
        bucket: "clipforge-test".to_string(),
        region: "us-east-1".to_string(),
        access_key: "test-access-key".to_string(),
        secret_key: "super-secret-key".to_string(),
        force_path_style: true,
    };

    // Read-only check
    let report = run_storage_check(&cfg, &StorageCheckOptions::default()).await;
    assert!(report.healthy);
    assert_eq!(report.backend, "s3");
    assert_eq!(state.lock().await.put_count, 0);
    assert!(
        report
            .checks
            .iter()
            .any(|c| c.name == "write_probe" && c.status == CheckStatus::Skipped)
    );

    // Opt-in write probe
    let report2 = run_storage_check(
        &cfg,
        &StorageCheckOptions {
            allow_write_probe: true,
            probe_prefix: "_clipforge_probe".to_string(),
        },
    )
    .await;
    assert!(report2.healthy);
    assert_eq!(state.lock().await.put_count, 1);
    assert_eq!(report2.capabilities.write, CapabilityStatus::Verified);
    assert_eq!(report2.capabilities.range_read, CapabilityStatus::Verified);
    assert_eq!(report2.capabilities.delete, CapabilityStatus::Verified);
    assert!(state.lock().await.objects.is_empty());
}
