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

#[tokio::test]
async fn rich_link_previews_og_twitter_oembed_xss_escaping_and_privacy() {
    let dir = TempDir::new().expect("tempdir");
    let (router, state) = setup_test_app(&dir, 1024, 10 * 1024 * 1024).await;

    let alice = register_user(&router, "embed_alice").await;

    let xss_title = "Clutch <script>alert(\"xss\")</script> & 'win'";
    let xss_notes = "Notes <img src=x onerror=\"evil()\">";

    // Create upload session with non-video bytes so ffmpeg fails gracefully and PNG fallback is exercised
    let (status, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/uploads",
        Some(&alice.token),
        Some(json!({
            "title": xss_title,
            "filename": "clutch.mp4",
            "content_type": "video/mp4",
            "total_bytes": 128,
            "width": 1920,
            "height": 1080,
            "duration_ms": 5000,
            "codec": "h264",
            "notes": xss_notes,
            "visibility": "public"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session: CreateUploadSessionResponse = serde_json::from_slice(&bytes).expect("session");

    let payload = vec![0xAAu8; 128];
    let put_req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/v1/uploads/{}/parts/1", session.session_id))
        .header(header::AUTHORIZATION, format!("Bearer {}", alice.token))
        .body(Body::from(payload))
        .expect("put part");
    let put_resp = router.clone().oneshot(put_req).await.expect("put");
    assert_eq!(put_resp.status(), StatusCode::OK);

    let (status, bytes) = json_req(
        &router,
        Method::POST,
        &format!("/api/v1/uploads/{}/complete", session.session_id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let upload_done: UploadStatusDto = serde_json::from_slice(&bytes).expect("complete");
    let clip = upload_done.clip.expect("clip");

    // Create public share
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
    let sid = &share.share_id;

    // 1. HTML meta tags on /c/{share_id} and /s/{share_id} with strict XSS escaping
    for route_prefix in ["/c", "/s"] {
        let (status, html_bytes) = json_req(
            &router,
            Method::GET,
            &format!("{route_prefix}/{sid}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let html = String::from_utf8(html_bytes).expect("utf8");

        assert!(html.contains("<meta property=\"og:type\" content=\"video.other\" />"));
        assert!(html.contains(&format!(
            "<meta property=\"og:url\" content=\"https://clips.test.local/c/{sid}\" />"
        )));
        assert!(html.contains(&format!(
            "<meta property=\"og:image\" content=\"https://clips.test.local/api/v1/shares/{sid}/thumbnail\" />"
        )));
        assert!(html.contains(&format!(
            "<meta property=\"og:video\" content=\"https://clips.test.local/api/v1/shares/{sid}/video\" />"
        )));
        assert!(html.contains(&format!(
            "<meta property=\"og:video:secure_url\" content=\"https://clips.test.local/api/v1/shares/{sid}/video\" />"
        )));
        assert!(html.contains("<meta property=\"og:video:type\" content=\"video/mp4\" />"));
        assert!(html.contains("<meta property=\"og:video:width\" content=\"1920\" />"));
        assert!(html.contains("<meta property=\"og:video:height\" content=\"1080\" />"));

        assert!(html.contains("<meta name=\"twitter:card\" content=\"player\" />"));
        assert!(html.contains(&format!(
            "<meta name=\"twitter:player\" content=\"https://clips.test.local/c/{sid}\" />"
        )));
        assert!(html.contains("<meta name=\"twitter:player:width\" content=\"1920\" />"));
        assert!(html.contains("<meta name=\"twitter:player:height\" content=\"1080\" />"));
        assert!(html.contains(&format!(
            "<meta name=\"twitter:player:stream\" content=\"https://clips.test.local/api/v1/shares/{sid}/video\" />"
        )));
        assert!(html.contains(
            "<meta name=\"twitter:player:stream:content_type\" content=\"video/mp4\" />"
        ));
        assert!(html.contains("<link rel=\"alternate\" type=\"application/json+oembed\""));

        // XSS payloads must be escaped, never raw
        assert!(!html.contains("<script>alert("));
        assert!(!html.contains("<img src=x"));
        assert!(
            html.contains(
                "&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt; &amp; &#x27;win&#x27;"
            )
        );
        assert!(html.contains("&lt;img src=x onerror=&quot;evil()&quot;&gt;"));
    }

    // 2. oEmbed endpoint
    let oembed_uri = format!("/api/v1/oembed?url=https://clips.test.local/c/{sid}");
    let (status, oembed_bytes) = json_req(&router, Method::GET, &oembed_uri, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let oembed: Value = serde_json::from_slice(&oembed_bytes).expect("oembed json");
    assert_eq!(oembed["type"], "video");
    assert_eq!(oembed["version"], "1.0");
    assert_eq!(oembed["title"], xss_title);
    assert_eq!(oembed["provider_name"], "ClipForge");
    assert_eq!(oembed["width"], 1920);
    assert_eq!(oembed["height"], 1080);
    assert_eq!(
        oembed["thumbnail_url"],
        format!("https://clips.test.local/api/v1/shares/{sid}/thumbnail")
    );
    assert!(
        oembed["html"]
            .as_str()
            .expect("html")
            .contains(&format!("src=\"https://clips.test.local/c/{sid}\""))
    );

    // 3. Embedded PNG fallback thumbnail (GET & HEAD)
    let thumb_uri = format!("/api/v1/shares/{sid}/thumbnail");
    let head_thumb = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri(&thumb_uri)
                .body(Body::empty())
                .expect("head thumb"),
        )
        .await
        .expect("head thumb resp");
    assert_eq!(head_thumb.status(), StatusCode::OK);
    assert_eq!(
        head_thumb.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    assert!(head_thumb.headers().get(header::CACHE_CONTROL).is_some());

    let get_thumb = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&thumb_uri)
                .body(Body::empty())
                .expect("get thumb"),
        )
        .await
        .expect("get thumb resp");
    assert_eq!(get_thumb.status(), StatusCode::OK);
    assert_eq!(
        get_thumb.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    assert!(get_thumb.headers().get(header::CACHE_CONTROL).is_some());
    let png_bytes = body_bytes(get_thumb).await;
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

    // 4. Public video HEAD & Range 206 + Cache-Control
    let video_uri = format!("/api/v1/shares/{sid}/video");
    let head_video = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri(&video_uri)
                .body(Body::empty())
                .expect("head video"),
        )
        .await
        .expect("head video resp");
    assert_eq!(head_video.status(), StatusCode::OK);
    assert_eq!(
        head_video.headers().get(header::CONTENT_TYPE).unwrap(),
        "video/mp4"
    );
    assert_eq!(
        head_video.headers().get(header::ACCEPT_RANGES).unwrap(),
        "bytes"
    );
    assert_eq!(
        head_video.headers().get(header::CONTENT_LENGTH).unwrap(),
        "128"
    );
    assert!(head_video.headers().get(header::CACHE_CONTROL).is_some());

    let range_video = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&video_uri)
                .header(header::RANGE, "bytes=0-63")
                .body(Body::empty())
                .expect("range video"),
        )
        .await
        .expect("range video resp");
    assert_eq!(range_video.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        range_video.headers().get(header::CONTENT_TYPE).unwrap(),
        "video/mp4"
    );
    assert_eq!(
        range_video.headers().get(header::ACCEPT_RANGES).unwrap(),
        "bytes"
    );
    assert_eq!(
        range_video.headers().get(header::CONTENT_LENGTH).unwrap(),
        "64"
    );
    assert!(range_video.headers().get(header::CACHE_CONTROL).is_some());

    // 5. Private share -> 403 on thumbnail/oembed, no metadata leak in HTML
    let (status, _) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/shares/{sid}/visibility"),
        Some(&alice.token),
        Some(json!({ "visibility": "private" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = json_req(&router, Method::GET, &thumb_uri, None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = json_req(&router, Method::GET, &oembed_uri, None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, html_bytes) =
        json_req(&router, Method::GET, &format!("/c/{sid}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let priv_html = String::from_utf8(html_bytes).expect("utf8");
    assert!(!priv_html.contains("Clutch"));
    assert!(!priv_html.contains(&alice.user_id));
    assert!(!priv_html.contains(&clip.id.to_string()));
    assert!(!priv_html.contains("og:video"));

    // 6. Revoked share -> 410 on thumbnail/oembed, no metadata leak in HTML
    let (status, _) = json_req(
        &router,
        Method::PATCH,
        &format!("/api/v1/shares/{sid}/visibility"),
        Some(&alice.token),
        Some(json!({ "visibility": "public" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = json_req(
        &router,
        Method::DELETE,
        &format!("/api/v1/shares/{sid}"),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = json_req(&router, Method::GET, &thumb_uri, None, None).await;
    assert_eq!(status, StatusCode::GONE);
    let (status, _) = json_req(&router, Method::GET, &oembed_uri, None, None).await;
    assert_eq!(status, StatusCode::GONE);

    let (status, html_bytes) =
        json_req(&router, Method::GET, &format!("/c/{sid}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let rev_html = String::from_utf8(html_bytes).expect("utf8");
    assert!(!rev_html.contains("Clutch"));
    assert!(!rev_html.contains(&alice.user_id));

    // 7. Expired share -> 410
    let expired_share = state
        .db
        .create_share(
            &alice.user_id,
            clip.id,
            "expired-test-token".to_string(),
            ShareVisibility::Public,
            Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        )
        .await
        .expect("expired share");
    let (status, _) = json_req(
        &router,
        Method::GET,
        &format!("/api/v1/shares/{}/thumbnail", expired_share.share_id),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    // 8. Invalid share -> 404
    let (status, _) = json_req(
        &router,
        Method::GET,
        "/api/v1/shares/nonexistent-token/thumbnail",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = json_req(
        &router,
        Method::GET,
        "/api/v1/oembed?url=https://clips.test.local/c/nonexistent-token",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ffmpeg_thumbnail_generation_and_short_clip_fallback() {
    let dir = TempDir::new().expect("tempdir");
    let mp4_path = dir.path().join("synthetic.mp4");

    // Generate a 0.5s synthetic MP4 using ffmpeg (exercises the 00:00:00 fallback for <1s clips)
    let ffmpeg_status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=320x180:d=0.5",
            "-pix_fmt",
            "yuv420p",
            "-y",
        ])
        .arg(&mp4_path)
        .status();

    let Ok(status) = ffmpeg_status else {
        // If ffmpeg is absent in a minimal CI container, skip this test cleanly.
        return;
    };
    if !status.success() {
        return;
    }

    let mp4_bytes = std::fs::read(&mp4_path).expect("read synthetic mp4");
    assert!(!mp4_bytes.is_empty());

    let (router, state) = setup_test_app(&dir, 4 * 1024 * 1024, 20 * 1024 * 1024).await;
    let alice = register_user(&router, "ffmpeg_alice").await;

    let (status, bytes) = json_req(
        &router,
        Method::POST,
        "/api/v1/uploads",
        Some(&alice.token),
        Some(json!({
            "title": "Synthetic FFmpeg Clip",
            "filename": "synthetic.mp4",
            "content_type": "video/mp4",
            "total_bytes": mp4_bytes.len(),
            "width": 320,
            "height": 180,
            "duration_ms": 500,
            "codec": "h264",
            "visibility": "public"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session: CreateUploadSessionResponse = serde_json::from_slice(&bytes).expect("session");

    let put_req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/api/v1/uploads/{}/parts/1", session.session_id))
        .header(header::AUTHORIZATION, format!("Bearer {}", alice.token))
        .body(Body::from(mp4_bytes))
        .expect("put part");
    let put_resp = router.clone().oneshot(put_req).await.expect("put");
    assert_eq!(put_resp.status(), StatusCode::OK);

    let (status, bytes) = json_req(
        &router,
        Method::POST,
        &format!("/api/v1/uploads/{}/complete", session.session_id),
        Some(&alice.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let done: UploadStatusDto = serde_json::from_slice(&bytes).expect("complete");
    let clip = done.clip.expect("clip");

    let thumb_asset = state
        .db
        .get_clip_asset_for_owner(
            &alice.user_id,
            clip.id,
            clipforge_db::ClipAssetKind::Thumbnail,
        )
        .await
        .expect("db")
        .expect("generated thumbnail asset");
    assert_eq!(thumb_asset.content_type, "image/jpeg");
    assert!(thumb_asset.storage_key.ends_with("/thumbnail.jpg"));
    assert!(thumb_asset.size_bytes > 0);

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
    assert_eq!(
        public_share.thumbnail_url.as_deref(),
        Some(
            format!(
                "https://clips.test.local/api/v1/shares/{}/thumbnail",
                share.share_id
            )
            .as_str()
        )
    );

    let thumb_uri = format!("/api/v1/shares/{}/thumbnail", share.share_id);
    let head_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri(&thumb_uri)
                .body(Body::empty())
                .expect("head"),
        )
        .await
        .expect("head resp");
    assert_eq!(head_resp.status(), StatusCode::OK);
    assert_eq!(
        head_resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg"
    );
    assert_eq!(
        head_resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=3600"
    );

    let get_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&thumb_uri)
                .body(Body::empty())
                .expect("get"),
        )
        .await
        .expect("get resp");
    assert_eq!(get_resp.status(), StatusCode::OK);
    assert_eq!(
        get_resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/jpeg"
    );
    let jpeg_bytes = body_bytes(get_resp).await;
    assert!(jpeg_bytes.starts_with(&[0xFF, 0xD8, 0xFF]));
}
