use std::sync::Arc;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use clipforge_api::{
    ApiTokenInfo, AuthLoginResponse, ClipDto, CreateShareResponse, CreateTokenResponse,
    CreateUploadSessionResponse, HealthDto, PublicShareDto, QuotaDto, ServerInfoDto,
    ShareVisibility, UploadSessionStatus, UploadStatusDto, UserInfo, UserRole,
};
use clipforge_auth::bootstrap_admin;
use clipforge_db::Database;
use clipforge_server::{AppState, ServerConfig, build_router};
use clipforge_storage::{StorageConfig, create_storage_backend};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

async fn setup_test_app(
    dir: &TempDir,
    chunk_size_bytes: u64,
    default_quota_bytes: u64,
) -> (axum::Router, AppState) {
    let db_path = dir.path().join("test.db");
    let storage_dir = dir.path().join("clips");
    let config = Arc::new(ServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        database_path: db_path.clone(),
        storage: StorageConfig::Local {
            root_dir: storage_dir,
        },
        max_upload_bytes: 64 * 1024 * 1024,
        chunk_size_bytes,
        default_quota_bytes,
        public_base_url: Some("https://clips.test.local".to_string()),
        web_dir: None,
        registration_enabled: true,
        session_ttl_hours: 24,
        admin_bootstrap_username: None,
        admin_bootstrap_password: None,
    });
    let db = Database::open(&db_path).await.expect("open db");
    let storage = create_storage_backend(&config.storage).expect("create local storage");
    let state = AppState::new(config, db, storage);
    let router = build_router(state.clone());
    (router, state)
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    use futures_util::StreamExt;
    let mut stream = resp.into_body().into_data_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("body chunk"));
    }
    out
}

async fn json_req(
    router: &axum::Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(tok) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    let req = if let Some(val) = body {
        builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&val).expect("json")))
            .expect("request")
    } else {
        builder.body(Body::empty()).expect("request")
    };
    let resp = router.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = body_bytes(resp).await;
    (status, bytes)
}

#[tokio::test]
async fn health_ready_info_and_openapi() {
    let dir = TempDir::new().expect("tempdir");
    let (router, _) = setup_test_app(&dir, 1024, 10 * 1024 * 1024).await;

    let (status, bytes) = json_req(&router, Method::GET, "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let health: HealthDto = serde_json::from_slice(&bytes).expect("health dto");
    assert_eq!(health.status, "ok");
    assert!(health.db_ok);
    assert!(health.storage_ok);

    let (status, _) = json_req(&router, Method::GET, "/ready", None, None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, bytes) = json_req(&router, Method::GET, "/api/v1/server/info", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let info: ServerInfoDto = serde_json::from_slice(&bytes).expect("server info");
    assert_eq!(info.api_version, "v1");
    assert_eq!(info.storage_backend, "local");

    let (status, bytes) = json_req(&router, Method::GET, "/api/v1/openapi.yaml", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let spec = String::from_utf8(bytes).expect("utf8");
    assert!(spec.contains("openapi: 3.1.0"));
}

#[tokio::test]
async fn auth_tokens_admin_and_unauthorized_denial() {
    let dir = TempDir::new().expect("tempdir");
    let (router, state) = setup_test_app(&dir, 1024, 10 * 1024 * 1024).await;

    // Unauthorized access denied
    let (status, _) = json_req(&router, Method::GET, "/api/v1/clips", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Bootstrap admin
    let admin = bootstrap_admin(&state.db, "rootadmin", "supersecret123", 10 * 1024 * 1024)
        .await
        .expect("bootstrap")
        .expect("admin created");
    assert_eq!(admin.role, UserRole::Admin);

    // Login as admin
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(json!({ "username": "rootadmin", "password": "supersecret123" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let admin_auth: AuthLoginResponse = serde_json::from_slice(&bytes).expect("admin auth");

    // Register normal user
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/auth/register",
        None,
        Some(json!({ "username": "alice", "password": "alicepassword1" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let alice_auth: AuthLoginResponse = serde_json::from_slice(&bytes).expect("alice auth");

    // Non-admin cannot access admin routes
    let (status, _) = json_req(
        &router,
        Method::GET,
        "/api/v1/admin/users",
        Some(&alice_auth.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Admin can list users and update Alice's quota
    let (status, bytes) = json_req(
        &router,
        Method::GET,
        "/api/v1/admin/users",
        Some(&admin_auth.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let users: Vec<UserInfo> = serde_json::from_slice(&bytes).expect("users");
    assert_eq!(users.len(), 2);

    let (status, bytes) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/admin/users/{}/quota", alice_auth.user_id),
        Some(&admin_auth.token),
        Some(json!({ "quota_limit_bytes": 2048 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let updated_alice: UserInfo = serde_json::from_slice(&bytes).expect("updated alice");
    assert_eq!(updated_alice.quota.expect("quota").limit_bytes, 2048);

    // Create, list, use, and revoke personal API token
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/auth/tokens",
        Some(&alice_auth.token),
        Some(json!({ "name": "ci-upload" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created_tok: CreateTokenResponse = serde_json::from_slice(&bytes).expect("created token");

    let (status, bytes) = json_req(
        &router,
        Method::GET,
        "/api/v1/auth/tokens",
        Some(&created_tok.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tokens: Vec<ApiTokenInfo> = serde_json::from_slice(&bytes).expect("tokens");
    assert!(tokens.iter().any(|t| t.id == created_tok.id));

    let (status, _) = json_req(
        &router,
        Method::DELETE,
        &format!("/api/v1/auth/tokens/{}", created_tok.id),
        Some(&alice_auth.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = json_req(
        &router,
        Method::GET,
        "/api/v1/auth/me",
        Some(&created_tok.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn streaming_upload_shares_range_isolation_quota_and_persistence() {
    let dir = TempDir::new().expect("tempdir");
    // 1024 byte chunk size, 4096 byte quota
    let (router, _) = setup_test_app(&dir, 1024, 4096).await;

    // Register Alice and Bob
    let (_, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/auth/register",
        None,
        Some(json!({ "username": "alice", "password": "alicepassword1" })),
    )
    .await;
    let alice: AuthLoginResponse = serde_json::from_slice(&bytes).expect("alice");

    let (_, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/auth/register",
        None,
        Some(json!({ "username": "bob", "password": "bobpassword123" })),
    )
    .await;
    let bob: AuthLoginResponse = serde_json::from_slice(&bytes).expect("bob");

    // Quota enforcement: requesting 5000 bytes > 4096 quota fails
    let (status, _) = json_req(
        &router,
        Method::POST,
        "/api/v1/uploads",
        Some(&alice.token),
        Some(json!({
            "title": "Too Big",
            "filename": "big.mp4",
            "total_bytes": 5000
        })),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    // Abort upload session releases reserved quota
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/uploads",
        Some(&alice.token),
        Some(json!({
            "title": "Aborted Clip",
            "filename": "abort.mp4",
            "total_bytes": 3000
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let aborted_sess: CreateUploadSessionResponse =
        serde_json::from_slice(&bytes).expect("abort sess");

    let (status, _) = json_req(
        &router,
        Method::DELETE,
        &format!("/api/v1/uploads/{}", aborted_sess.session_id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Now Alice creates a 2048-byte 2-part streaming upload
    let payload: Vec<u8> = (0..2048).map(|i| (i % 251) as u8).collect();
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/uploads",
        Some(&alice.token),
        Some(json!({
            "title": "Clutch Play",
            "filename": "clutch.mp4",
            "total_bytes": payload.len(),
            "visibility": "public"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let sess: CreateUploadSessionResponse = serde_json::from_slice(&bytes).expect("sess");
    assert_eq!(sess.total_parts, 2);

    // Bob cannot inspect Alice's upload session
    let (status, _) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/uploads/{}", sess.session_id),
        Some(&bob.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Upload part 1 and part 2
    for (idx, chunk) in payload.chunks(1024).enumerate() {
        let part_num = idx + 1;
        let req = Request::builder()
            .method(Method::PUT)
            .uri(format!(
                "/api/v1/uploads/{}/parts/{part_num}",
                sess.session_id
            ))
            .header(header::AUTHORIZATION, format!("Bearer {}", alice.token))
            .body(Body::from(chunk.to_vec()))
            .expect("part req");
        let resp = router.clone().oneshot(req).await.expect("part resp");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Complete upload
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        &format!("/api/v1/uploads/{}/complete", sess.session_id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let completed: UploadStatusDto = serde_json::from_slice(&bytes).expect("completed");
    assert_eq!(completed.status, UploadSessionStatus::Completed);
    let clip = completed.clip.expect("clip dto");

    // Verify quota updated
    let (status, bytes) = json_req(
        &router,
        Method::GET,
        "/api/v1/server/quota",
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let quota: QuotaDto = serde_json::from_slice(&bytes).expect("quota");
    assert_eq!(quota.used_bytes, 2048);
    assert_eq!(quota.clip_count, 1);

    // Cross-user isolation: Bob cannot read, rename, or delete Alice's clip
    let (status, _) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&bob.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&bob.token),
        Some(json!({ "title": "Hacked" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = json_req(
        &router,
        Method::DELETE,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&bob.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Alice renames clip
    let (status, bytes) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&alice.token),
        Some(json!({ "title": "Clutch Play (Renamed)" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let renamed: ClipDto = serde_json::from_slice(&bytes).expect("renamed");
    assert_eq!(renamed.title, "Clutch Play (Renamed)");

    // Create share link
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        &format!("/api/v1/clips/{}/shares", clip.id),
        Some(&alice.token),
        Some(json!({ "visibility": "public" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let share: CreateShareResponse = serde_json::from_slice(&bytes).expect("share");
    assert_eq!(share.visibility, ShareVisibility::Public);

    // Public share lookup + player HTML + Range video streaming
    let (status, bytes) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/shares/{}", share.share_id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let public_share: PublicShareDto = serde_json::from_slice(&bytes).expect("public share");
    assert_eq!(public_share.title, "Clutch Play (Renamed)");

    let (status, html_bytes) = json_req(
        &router,
        Method::GET,
        &format!("/c/{}", share.share_id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&html_bytes).contains("<video"));

    // Range request: bytes=100-199
    let range_req = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/v1/shares/{}/video", share.share_id))
        .header(header::RANGE, "bytes=100-199")
        .body(Body::empty())
        .expect("range req");
    let range_resp = router.clone().oneshot(range_req).await.expect("range resp");
    assert_eq!(range_resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        range_resp
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()),
        Some("bytes 100-199/2048")
    );
    let range_bytes = body_bytes(range_resp).await;
    assert_eq!(range_bytes, &payload[100..=199]);

    // Invalid range -> 416
    let bad_range_req = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/v1/shares/{}/video", share.share_id))
        .header(header::RANGE, "bytes=5000-6000")
        .body(Body::empty())
        .expect("bad range");
    let bad_range_resp = router
        .clone()
        .oneshot(bad_range_req)
        .await
        .expect("bad range resp");
    assert_eq!(bad_range_resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // Persistence across DB reopen
    drop(router);
    let (reopened_router, _) = setup_test_app(&dir, 1024, 4096).await;
    let (status, bytes) = json_req(
        &reopened_router,
        Method::GET,
        "/api/v1/clips",
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed: Vec<ClipDto> = serde_json::from_slice(&bytes).expect("listed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, clip.id);

    // Revoke share -> 410 Gone
    let (status, _) = json_req(
        &reopened_router,
        Method::DELETE,
        &format!("/api/v1/shares/{}", share.share_id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = json_req(
        &reopened_router,
        Method::GET,
        &format!("/api/v1/shares/{}", share.share_id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    // Delete clip -> 204 and quota freed
    let (status, _) = json_req(
        &reopened_router,
        Method::DELETE,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, bytes) = json_req(
        &reopened_router,
        Method::GET,
        "/api/v1/server/quota",
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let quota_after: QuotaDto = serde_json::from_slice(&bytes).expect("quota after");
    assert_eq!(quota_after.used_bytes, 0);
    assert_eq!(quota_after.clip_count, 0);
}

async fn register_user(router: &axum::Router, username: &str) -> AuthLoginResponse {
    let (status, bytes) = json_req(
        router,
        Method::POST,
        "/api/v1/auth/register",
        None,
        Some(json!({ "username": username, "password": "password1234" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    serde_json::from_slice(&bytes).expect("register auth")
}

async fn upload_test_clip(
    router: &axum::Router,
    token: &str,
    title: &str,
    payload: &[u8],
    visibility: ShareVisibility,
) -> ClipDto {
    let (status, bytes) = json_req(
        router,
        Method::POST,
        "/api/v1/uploads",
        Some(token),
        Some(json!({
            "title": title,
            "filename": "clip.mp4",
            "total_bytes": payload.len(),
            "visibility": visibility,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session: CreateUploadSessionResponse = serde_json::from_slice(&bytes).expect("session");

    let put_req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/v1/uploads/{}/parts/1", session.session_id))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(payload.to_vec()))
        .expect("put part");
    let put_resp = router
        .clone()
        .oneshot(put_req)
        .await
        .expect("put part resp");
    assert_eq!(put_resp.status(), StatusCode::OK);

    let (status, bytes) = json_req(
        router,
        Method::POST,
        &format!("/api/v1/uploads/{}/complete", session.session_id),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let completed: UploadStatusDto = serde_json::from_slice(&bytes).expect("complete");
    completed.clip.expect("completed clip")
}

#[tokio::test]
async fn visibility_update_syncs_clip_and_share_and_denies_cross_user() {
    let dir = TempDir::new().expect("tempdir");
    let (router, _) = setup_test_app(&dir, 4096, 1024 * 1024).await;

    let alice = register_user(&router, "alice").await;
    let bob = register_user(&router, "bob").await;

    let clip = upload_test_clip(
        &router,
        &alice.token,
        "Visibility Test",
        b"video-bytes-1234",
        ShareVisibility::Public,
    )
    .await;

    // Create share link
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        &format!("/api/v1/clips/{}/shares", clip.id),
        Some(&alice.token),
        Some(json!({ "visibility": "public" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let share: CreateShareResponse = serde_json::from_slice(&bytes).expect("share");

    // Public share works anonymously
    let (status, _) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/shares/{}", share.share_id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Bob cannot update Alice's clip or share visibility
    let (status, _) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&bob.token),
        Some(json!({ "visibility": "private" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/shares/{}/visibility", share.share_id),
        Some(&bob.token),
        Some(json!({ "visibility": "private" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Alice toggles clip visibility to private via PATCH /api/v1/clips/{id}
    let (status, bytes) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&alice.token),
        Some(json!({ "visibility": "private" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let updated: ClipDto = serde_json::from_slice(&bytes).expect("updated clip");
    assert_eq!(updated.visibility, ShareVisibility::Private);

    // Public share is now denied (403 Forbidden from share.visibility == Private)
    let (status, _) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/shares/{}", share.share_id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // GET /api/v1/clips persists private visibility
    let (status, bytes) = json_req(
        &router,
        Method::GET,
        "/api/v1/clips",
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let clips: Vec<ClipDto> = serde_json::from_slice(&bytes).expect("clips");
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].visibility, ShareVisibility::Private);
    assert_eq!(clips[0].share_id.as_deref(), Some(share.share_id.as_str()));

    // Alice toggles back to public via PATCH /api/v1/shares/{token}/visibility
    let (status, bytes) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/shares/{}/visibility", share.share_id),
        Some(&alice.token),
        Some(json!({ "visibility": "public" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let share_resp: CreateShareResponse = serde_json::from_slice(&bytes).expect("share resp");
    assert_eq!(share_resp.visibility, ShareVisibility::Public);

    // Clip and public share both reflect public again
    let (status, bytes) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fetched: ClipDto = serde_json::from_slice(&bytes).expect("fetched");
    assert_eq!(fetched.visibility, ShareVisibility::Public);

    let (status, bytes) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/shares/{}", share.share_id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pub_share: PublicShareDto = serde_json::from_slice(&bytes).expect("pub share");
    assert_eq!(pub_share.visibility, ShareVisibility::Public);
}

struct FaultyDeleteStorage {
    inner: Arc<dyn clipforge_storage::StorageBackend>,
    fail_delete_for_substring: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl clipforge_storage::StorageBackend for FaultyDeleteStorage {
    async fn put_object(
        &self,
        key: &str,
        stream: clipforge_storage::ByteStream,
        size_bytes: Option<u64>,
        content_type: &str,
    ) -> clipforge_storage::Result<clipforge_storage::ObjectMeta> {
        self.inner
            .put_object(key, stream, size_bytes, content_type)
            .await
    }

    async fn put_bytes(
        &self,
        key: &str,
        data: bytes::Bytes,
        content_type: &str,
    ) -> clipforge_storage::Result<clipforge_storage::ObjectMeta> {
        self.inner.put_bytes(key, data, content_type).await
    }

    async fn put_file(
        &self,
        key: &str,
        source_path: &std::path::Path,
        size_bytes: u64,
        content_type: &str,
    ) -> clipforge_storage::Result<clipforge_storage::ObjectMeta> {
        self.inner
            .put_file(key, source_path, size_bytes, content_type)
            .await
    }

    async fn get_object_range(
        &self,
        key: &str,
        start: u64,
        end_inclusive: Option<u64>,
    ) -> clipforge_storage::Result<clipforge_storage::RangeObject> {
        self.inner.get_object_range(key, start, end_inclusive).await
    }

    async fn head_object(
        &self,
        key: &str,
    ) -> clipforge_storage::Result<clipforge_storage::ObjectMeta> {
        self.inner.head_object(key).await
    }

    async fn delete_object(&self, key: &str) -> clipforge_storage::Result<()> {
        let should_fail = self
            .fail_delete_for_substring
            .lock()
            .expect("lock")
            .as_ref()
            .is_some_and(|pat| key.contains(pat));
        if should_fail {
            return Err(clipforge_storage::StorageError::Unavailable(
                "simulated S3 delete failure".to_string(),
            ));
        }
        self.inner.delete_object(key).await
    }

    async fn create_multipart(
        &self,
        key: &str,
        content_type: &str,
    ) -> clipforge_storage::Result<clipforge_storage::MultipartUpload> {
        self.inner.create_multipart(key, content_type).await
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        data: bytes::Bytes,
    ) -> clipforge_storage::Result<clipforge_storage::CompletedPart> {
        self.inner
            .upload_part(key, upload_id, part_number, data)
            .await
    }

    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[clipforge_storage::CompletedPart],
    ) -> clipforge_storage::Result<clipforge_storage::ObjectMeta> {
        self.inner.complete_multipart(key, upload_id, parts).await
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> clipforge_storage::Result<()> {
        self.inner.abort_multipart(key, upload_id).await
    }

    async fn health_check(&self) -> clipforge_storage::Result<()> {
        self.inner.health_check().await
    }
}

#[tokio::test]
async fn delete_clip_purges_storage_invalidates_shares_handles_partial_failure_and_repeats() {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let storage_dir = dir.path().join("clips");
    let config = Arc::new(ServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        database_path: db_path.clone(),
        storage: StorageConfig::Local {
            root_dir: storage_dir,
        },
        max_upload_bytes: 64 * 1024 * 1024,
        chunk_size_bytes: 4096,
        default_quota_bytes: 1024 * 1024,
        public_base_url: Some("https://clips.test.local".to_string()),
        web_dir: None,
        registration_enabled: true,
        session_ttl_hours: 24,
        admin_bootstrap_username: None,
        admin_bootstrap_password: None,
    });
    let db = Database::open(&db_path).await.expect("open db");
    let inner_storage = create_storage_backend(&config.storage).expect("local storage");
    let faulty_storage = Arc::new(FaultyDeleteStorage {
        inner: Arc::clone(&inner_storage),
        fail_delete_for_substring: std::sync::Mutex::new(Some("thumbnail".to_string())),
    });
    let state = AppState::new(config, db.clone(), faulty_storage.clone());
    let router = build_router(state);

    let alice = register_user(&router, "alice").await;
    let bob = register_user(&router, "bob").await;

    let clip = upload_test_clip(
        &router,
        &alice.token,
        "Delete Lifecycle",
        b"0123456789abcdef",
        ShareVisibility::Public,
    )
    .await;

    // Attach a thumbnail asset so the clip has 2 storage objects (original + thumbnail)
    let thumb_key = clipforge_storage::build_clip_object_key(
        &alice.user_id,
        &clip.id.to_string(),
        clipforge_storage::ClipObjectKind::Thumbnail,
    )
    .expect("thumb key");
    inner_storage
        .put_bytes(
            &thumb_key,
            bytes::Bytes::from_static(b"thumb"),
            "image/jpeg",
        )
        .await
        .expect("put thumb");
    db.upsert_clip_asset_for_owner(
        &alice.user_id,
        clipforge_db::NewClipAsset {
            id: uuid::Uuid::new_v4().to_string(),
            clip_id: clip.id,
            owner_id: alice.user_id.clone(),
            asset_kind: clipforge_db::ClipAssetKind::Thumbnail,
            storage_key: thumb_key.clone(),
            content_type: "image/jpeg".to_string(),
            size_bytes: 5,
            sha256: None,
        },
    )
    .await
    .expect("upsert thumb");

    let assets_before = db
        .list_clip_assets_for_owner(&alice.user_id, clip.id)
        .await
        .expect("assets");
    assert_eq!(assets_before.len(), 2);

    // Create a public share link
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        &format!("/api/v1/clips/{}/shares", clip.id),
        Some(&alice.token),
        Some(json!({ "visibility": "public" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let share: CreateShareResponse = serde_json::from_slice(&bytes).expect("share");

    // Cross-user delete denied
    let (status, _) = json_req(
        &router,
        Method::DELETE,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&bob.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Partial storage failure (thumbnail delete fails) -> 503 Service Unavailable,
    // DB clip, assets, share, and quota remain intact and recoverable.
    let (status, _) = json_req(
        &router,
        Method::DELETE,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // Clip and share still exist in DB/API
    let (status, _) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/shares/{}", share.share_id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(inner_storage.head_object(&thumb_key).await.is_ok());

    // Clear the storage fault and retry DELETE -> 204 No Content
    *faulty_storage
        .fail_delete_for_substring
        .lock()
        .expect("lock") = None;
    let (status, _) = json_req(
        &router,
        Method::DELETE,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Both storage objects are gone
    for asset in &assets_before {
        assert!(
            matches!(
                inner_storage.head_object(&asset.storage_key).await,
                Err(clipforge_storage::StorageError::NotFound(_))
            ),
            "expected storage object {} to be deleted",
            asset.storage_key
        );
    }

    // Clip is gone from API & DB, share link is invalidated (404), quota is 0
    let (status, bytes) = json_req(
        &router,
        Method::GET,
        "/api/v1/clips",
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let clips: Vec<ClipDto> = serde_json::from_slice(&bytes).expect("clips");
    assert!(clips.is_empty());

    let (status, _) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/shares/{}", share.share_id),
        None,
        None,
    )
    .await;
    assert!(status == StatusCode::NOT_FOUND || status == StatusCode::GONE);

    let (status, bytes) = json_req(
        &router,
        Method::GET,
        "/api/v1/server/quota",
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let quota: QuotaDto = serde_json::from_slice(&bytes).expect("quota");
    assert_eq!(quota.used_bytes, 0);
    assert_eq!(quota.clip_count, 0);

    // Repeated DELETE returns clean 404 without corrupting quota
    let (status, _) = json_req(
        &router,
        Method::DELETE,
        &format!("/api/v1/clips/{}", clip.id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
