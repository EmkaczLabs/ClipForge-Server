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
