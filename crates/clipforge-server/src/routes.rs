//! Axum HTTP router and `/api/v1` handlers for `clipforge-server`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, FromRequestParts, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use clipforge_api::{
    ApiTokenInfo, AudioTrackInfo, AuthLoginRequest, AuthLoginResponse, ClipDto, CreateShareRequest,
    CreateShareResponse, CreateTokenRequest, CreateTokenResponse, CreateUploadSessionRequest,
    CreateUploadSessionResponse, GameInfo, HealthDto, OPENAPI_SPEC_YAML, PaginationQuery,
    PublicShareDto, QuotaDto, RegisterRequest, ServerInfoDto, ShareVisibility, UpdateClipRequest,
    UploadSessionStatus, UploadStatusDto, UserInfo, UserRole,
};
use clipforge_auth::{
    AuthenticatedUser, authenticate_password, create_user, extract_raw_credential,
    generate_share_token, issue_api_token, list_my_tokens, require_admin, revoke_my_token,
};
use clipforge_db::{
    ClipAssetKind, Database, NewClip, NewClipAsset, NewUploadSession, ShareLookupOutcome,
    ShareRecord, UploadMetadata,
};
use clipforge_storage::{
    ClipObjectKind, CompletedPart, StorageBackend, StorageConfig, build_clip_object_key,
};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::error::ServerError;
use crate::range::{head_object_response, stream_object_response};

const EMBEDDED_PLAYER_HTML: &str = include_str!("../../../web/index.html");

/// Shared application state for Axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub db: Database,
    pub storage: Arc<dyn StorageBackend>,
    pub started_at: Instant,
    pub upload_parts: Arc<Mutex<HashMap<String, Vec<CompletedPart>>>>,
}

impl AppState {
    /// Creates a new [`AppState`].
    pub fn new(config: Arc<ServerConfig>, db: Database, storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            config,
            db,
            storage,
            started_at: Instant::now(),
            upload_parts: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Builds the complete Axum [`Router`] for `clipforge-server`.
pub fn build_router(state: AppState) -> Router {
    let direct_upload_routes = Router::new()
        .route("/api/v1/clips", post(upload_clip_multipart))
        .route(
            "/api/v1/uploads/{id}/parts/{part_number}",
            put(upload_session_part),
        )
        .layer(DefaultBodyLimit::disable());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        // Health, readiness, info & OpenAPI
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/api/v1/health", get(health))
        .route("/api/v1/server/info", get(server_info))
        .route("/api/v1/server/quota", get(get_quota))
        .route("/api/v1/quota", get(get_quota))
        .route("/api/v1/openapi.yaml", get(openapi_yaml))
        .route("/api/v1/openapi.json", get(openapi_json))
        // Browser share player pages
        .route("/c/{share_id}", get(serve_player_page))
        .route("/s/{share_id}", get(serve_player_page))
        // Auth & personal API tokens
        .route("/api/v1/auth/register", post(register))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/me", get(auth_me))
        .route("/api/v1/auth/tokens", get(list_tokens).post(create_token))
        .route("/api/v1/auth/tokens/{id}", delete(revoke_token))
        // Streaming / chunked uploads
        .route("/api/v1/uploads", post(create_upload_session))
        .route(
            "/api/v1/uploads/{id}",
            get(get_upload_status).delete(abort_upload_session),
        )
        .route(
            "/api/v1/uploads/{id}/complete",
            post(complete_upload_session),
        )
        // Clips
        .merge(direct_upload_routes)
        .route("/api/v1/clips", get(list_clips))
        .route(
            "/api/v1/clips/{id}",
            get(get_clip).patch(update_clip).delete(delete_clip),
        )
        .route("/api/v1/clips/{id}/thumbnail", get(get_clip_thumbnail))
        .route(
            "/api/v1/clips/{id}/video",
            get(stream_clip_video).head(head_clip_video),
        )
        .route("/api/v1/clips/{id}/shares", post(create_clip_share))
        // Shares
        .route("/api/v1/shares", get(list_user_shares))
        .route(
            "/api/v1/shares/{token}",
            get(get_public_share).delete(revoke_share),
        )
        .route(
            "/api/v1/shares/{token}/visibility",
            patch(update_share_visibility),
        )
        .route(
            "/api/v1/shares/{token}/video",
            get(stream_share_video).head(head_share_video),
        )
        // Admin
        .route(
            "/api/v1/admin/users",
            get(admin_list_users).post(admin_create_user),
        )
        .route("/api/v1/admin/users/{id}/quota", patch(admin_update_quota))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

impl FromRequestParts<AppState> for AuthenticatedUser {
    type Rejection = ServerError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        let api_key_header = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok());
        let query_token = parts.uri.query().and_then(extract_query_token);

        let raw = extract_raw_credential(auth_header, api_key_header, query_token.as_deref())
            .ok_or_else(|| {
                ServerError::Unauthorized("missing authentication credentials".to_string())
            })?;

        Ok(clipforge_auth::verify_api_token(&state.db, &raw).await?)
    }
}

fn extract_query_token(query: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == "token"
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
}

async fn try_authenticate(
    headers: &HeaderMap,
    query_token: Option<&str>,
    state: &AppState,
) -> Result<Option<AuthenticatedUser>, ServerError> {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let api_key_header = headers.get("x-api-key").and_then(|v| v.to_str().ok());

    let Some(raw) = extract_raw_credential(auth_header, api_key_header, query_token) else {
        return Ok(None);
    };
    match clipforge_auth::verify_api_token(&state.db, &raw).await {
        Ok(u) => Ok(Some(u)),
        Err(
            clipforge_auth::AuthError::InvalidCredentials
            | clipforge_auth::AuthError::TokenExpired
            | clipforge_auth::AuthError::TokenRevoked,
        ) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn resolve_base_url(config: &ServerConfig, headers: &HeaderMap) -> String {
    if let Some(base) = &config.public_base_url {
        return base.clone();
    }
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok());
    if let Some(host) = host {
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http");
        return format!("{proto}://{host}");
    }
    String::new()
}

fn build_share_url(config: &ServerConfig, headers: &HeaderMap, share_id: &str) -> String {
    let base = resolve_base_url(config, headers);
    if base.is_empty() {
        format!("/c/{share_id}")
    } else {
        format!("{base}/c/{share_id}")
    }
}

fn enrich_clip_dto(mut clip: ClipDto, config: &ServerConfig, headers: &HeaderMap) -> ClipDto {
    let base = resolve_base_url(config, headers);
    let prefix = if base.is_empty() { String::new() } else { base };
    clip.stream_url = Some(format!("{prefix}/api/v1/clips/{}/video", clip.id));
    if let Some(share_id) = &clip.share_id {
        clip.share_url = Some(build_share_url(config, headers, share_id));
    }
    clip
}

// ---------------------------------------------------------------------------
// Health, Readiness, Server Info & OpenAPI
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> Json<HealthDto> {
    let db_ok = state.db.health_check().await.is_ok();
    let storage_ok = state.storage.health_check().await.is_ok();
    Json(HealthDto {
        status: if db_ok && storage_ok {
            "ok"
        } else {
            "degraded"
        }
        .to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        db_ok,
        storage_ok,
        uptime_secs: state.started_at.elapsed().as_secs(),
    })
}

async fn ready(State(state): State<AppState>) -> Result<Json<HealthDto>, ServerError> {
    let db_ok = state.db.health_check().await.is_ok();
    let storage_ok = state.storage.health_check().await.is_ok();
    if !db_ok || !storage_ok {
        return Err(ServerError::ServiceUnavailable(format!(
            "server not ready (db_ok={db_ok}, storage_ok={storage_ok})"
        )));
    }
    Ok(Json(HealthDto {
        status: "ready".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        db_ok,
        storage_ok,
        uptime_secs: state.started_at.elapsed().as_secs(),
    }))
}

async fn server_info(State(state): State<AppState>) -> Json<ServerInfoDto> {
    let storage_backend = match &state.config.storage {
        StorageConfig::Local { .. } => "local",
        StorageConfig::S3 { .. } => "s3",
    };
    Json(ServerInfoDto {
        name: "ClipForge Server".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        api_version: clipforge_api::API_VERSION.to_string(),
        storage_backend: storage_backend.to_string(),
        max_upload_bytes: state.config.max_upload_bytes,
        chunk_size_bytes: state.config.chunk_size_bytes,
        registration_enabled: state.config.registration_enabled,
        public_base_url: state.config.public_base_url.clone(),
    })
}

async fn get_quota(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<QuotaDto>, ServerError> {
    let quota = state.db.get_storage_usage(&user.user_id).await?;
    Ok(Json(quota))
}

async fn openapi_yaml() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "application/yaml; charset=utf-8")],
        OPENAPI_SPEC_YAML,
    )
}

async fn openapi_json() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "ClipForge Server API",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "spec_yaml_url": "/api/v1/openapi.yaml"
    }))
}

async fn serve_player_page(State(state): State<AppState>) -> Html<String> {
    if let Some(web_dir) = &state.config.web_dir {
        let path = web_dir.join("index.html");
        if let Ok(contents) = tokio::fs::read_to_string(&path).await {
            return Html(contents);
        }
    }
    Html(EMBEDDED_PLAYER_HTML.to_string())
}

// ---------------------------------------------------------------------------
// Auth & Personal API Tokens
// ---------------------------------------------------------------------------

async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<AuthLoginResponse>), ServerError> {
    if !state.config.registration_enabled {
        return Err(ServerError::Forbidden(
            "user registration is disabled on this server".to_string(),
        ));
    }

    let user = create_user(
        &state.db,
        &req.username,
        &req.password,
        req.display_name,
        UserRole::User,
        state.config.default_quota_bytes,
    )
    .await?;

    let expires_in_days = u32::try_from((state.config.session_ttl_hours.max(24) + 23) / 24)
        .unwrap_or(30)
        .max(1);
    let token_resp = issue_api_token(
        &state.db,
        &user.id,
        CreateTokenRequest {
            name: "session".to_string(),
            scopes: vec![],
            expires_in_days: Some(expires_in_days),
        },
    )
    .await?;

    let quota = state.db.get_storage_usage(&user.id).await.ok();
    let expires_at = token_resp
        .expires_at
        .unwrap_or_else(|| Utc::now() + Duration::days(i64::from(expires_in_days)));

    Ok((
        StatusCode::CREATED,
        Json(AuthLoginResponse {
            user_id: user.id.clone(),
            username: user.username.clone(),
            token: token_resp.token,
            expires_at,
            user: Some(user.to_user_info(quota)),
        }),
    ))
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<AuthLoginRequest>,
) -> Result<Json<AuthLoginResponse>, ServerError> {
    let user = authenticate_password(&state.db, &req.username, &req.password).await?;
    let expires_in_days = u32::try_from((state.config.session_ttl_hours.max(24) + 23) / 24)
        .unwrap_or(30)
        .max(1);
    let token_resp = issue_api_token(
        &state.db,
        &user.id,
        CreateTokenRequest {
            name: "session".to_string(),
            scopes: vec![],
            expires_in_days: Some(expires_in_days),
        },
    )
    .await?;

    let quota = state.db.get_storage_usage(&user.id).await.ok();
    let expires_at = token_resp
        .expires_at
        .unwrap_or_else(|| Utc::now() + Duration::days(i64::from(expires_in_days)));

    Ok(Json(AuthLoginResponse {
        user_id: user.id.clone(),
        username: user.username.clone(),
        token: token_resp.token,
        expires_at,
        user: Some(user.to_user_info(quota)),
    }))
}

async fn logout(_user: AuthenticatedUser) -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn auth_me(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<UserInfo>, ServerError> {
    let record = state
        .db
        .get_user_by_id(&user.user_id)
        .await?
        .ok_or_else(|| ServerError::NotFound("user not found".to_string()))?;
    let quota = state.db.get_storage_usage(&user.user_id).await.ok();
    Ok(Json(record.to_user_info(quota)))
}

async fn list_tokens(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<ApiTokenInfo>>, ServerError> {
    let tokens = list_my_tokens(&state.db, &user).await?;
    Ok(Json(tokens))
}

async fn create_token(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(req): Json<CreateTokenRequest>,
) -> Result<(StatusCode, Json<CreateTokenResponse>), ServerError> {
    let resp = issue_api_token(&state.db, &user.user_id, req).await?;
    Ok((StatusCode::CREATED, Json(resp)))
}

async fn revoke_token(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<StatusCode, ServerError> {
    if revoke_my_token(&state.db, &user, &id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ServerError::NotFound("api token not found".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Streaming / Chunked Upload Sessions
// ---------------------------------------------------------------------------

async fn create_upload_session(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Json(req): Json<CreateUploadSessionRequest>,
) -> Result<(StatusCode, Json<CreateUploadSessionResponse>), ServerError> {
    if req.total_bytes == 0 {
        return Err(ServerError::BadRequest(
            "total_bytes must be greater than zero".to_string(),
        ));
    }
    if req.total_bytes > state.config.max_upload_bytes {
        return Err(ServerError::PayloadTooLarge(format!(
            "upload exceeds max_upload_bytes ({})",
            state.config.max_upload_bytes
        )));
    }
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(ServerError::BadRequest("title cannot be empty".to_string()));
    }

    let clip_id = req.clip_id.unwrap_or_else(Uuid::new_v4);
    let session_id = Uuid::new_v4().to_string();
    let part_size_bytes = state.config.chunk_size_bytes;
    let total_parts = u32::try_from(req.total_bytes.div_ceil(part_size_bytes).max(1))
        .map_err(|_| ServerError::BadRequest("upload requires too many parts".to_string()))?;

    let clip_id_str = clip_id.to_string();
    let storage_key = build_clip_object_key(&user.user_id, &clip_id_str, ClipObjectKind::Original)
        .map_err(|e| ServerError::BadRequest(e.to_string()))?;
    let content_type = req
        .content_type
        .clone()
        .unwrap_or_else(|| content_type_for_filename(&req.filename).to_string());
    let expires_at = Utc::now() + Duration::hours(24);

    let session = state
        .db
        .create_upload_session(NewUploadSession {
            session_id: session_id.clone(),
            user_id: user.user_id.clone(),
            clip_id,
            title,
            filename: req.filename,
            content_type: content_type.clone(),
            sha256: req.sha256,
            visibility: req.visibility,
            metadata: UploadMetadata {
                game: req.game,
                duration_ms: req.duration_ms,
                width: req.width,
                height: req.height,
                fps: req.fps,
                codec: req.codec,
                audio_tracks: req.audio_tracks,
                tags: req.tags,
                notes: req.notes,
                created_at: req.created_at,
            },
            storage_key: storage_key.clone(),
            total_bytes: req.total_bytes,
            part_size_bytes,
            total_parts,
            expires_at,
        })
        .await?;

    let mp = match state
        .storage
        .create_multipart(&storage_key, &content_type)
        .await
    {
        Ok(mp) => mp,
        Err(err) => {
            let _ = state
                .db
                .fail_upload_session(&user.user_id, &session_id)
                .await;
            return Err(err.into());
        }
    };

    if let Err(err) = state
        .db
        .set_upload_multipart_id(&user.user_id, &session_id, &mp.upload_id)
        .await
    {
        let _ = state
            .storage
            .abort_multipart(&storage_key, &mp.upload_id)
            .await;
        let _ = state
            .db
            .fail_upload_session(&user.user_id, &session_id)
            .await;
        return Err(err.into());
    }

    let base = resolve_base_url(&state.config, &headers);
    let upload_url = if base.is_empty() {
        format!("/api/v1/uploads/{session_id}")
    } else {
        format!("{base}/api/v1/uploads/{session_id}")
    };

    Ok((
        StatusCode::CREATED,
        Json(CreateUploadSessionResponse {
            session_id: session.session_id,
            clip_id: session.clip_id,
            upload_url,
            part_size_bytes: session.part_size_bytes,
            total_bytes: session.total_bytes,
            total_parts: session.total_parts,
            expires_at: session.expires_at,
        }),
    ))
}

async fn upload_session_part(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((session_id, part_number)): Path<(String, u32)>,
    body: Bytes,
) -> Result<Json<UploadStatusDto>, ServerError> {
    if body.is_empty() {
        return Err(ServerError::BadRequest(
            "part body cannot be empty".to_string(),
        ));
    }
    let max_part = state.config.chunk_size_bytes.saturating_add(1024 * 1024);
    if (body.len() as u64) > max_part {
        return Err(ServerError::PayloadTooLarge(format!(
            "part size {} exceeds limit {}",
            body.len(),
            max_part
        )));
    }

    let session = state
        .db
        .get_upload_session_for_owner(&user.user_id, &session_id)
        .await?
        .ok_or_else(|| ServerError::NotFound("upload session not found".to_string()))?;

    let multipart_id = session.multipart_upload_id.as_deref().ok_or_else(|| {
        ServerError::Conflict("upload session has no active multipart upload".to_string())
    })?;

    let part_len = body.len() as u64;
    let completed = state
        .storage
        .upload_part(&session.storage_key, multipart_id, part_number, body)
        .await?;

    {
        let mut guard = state
            .upload_parts
            .lock()
            .map_err(|_| ServerError::Internal("upload parts lock poisoned".to_string()))?;
        let parts = guard.entry(session_id.clone()).or_default();
        if let Some(existing) = parts.iter_mut().find(|p| p.part_number == part_number) {
            *existing = completed;
        } else {
            parts.push(completed);
        }
    }

    let updated = state
        .db
        .record_upload_part(&user.user_id, &session_id, part_number, part_len)
        .await?;

    Ok(Json(UploadStatusDto {
        session_id: updated.session_id,
        clip_id: updated.clip_id,
        status: updated.status,
        bytes_received: updated.bytes_received,
        total_bytes: updated.total_bytes,
        uploaded_parts: updated.uploaded_parts,
        total_parts: updated.total_parts,
        expires_at: updated.expires_at,
        clip: None,
    }))
}

async fn get_upload_status(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<UploadStatusDto>, ServerError> {
    let session = state
        .db
        .get_upload_session_for_owner(&user.user_id, &session_id)
        .await?
        .ok_or_else(|| ServerError::NotFound("upload session not found".to_string()))?;

    let clip = if session.status == UploadSessionStatus::Completed {
        state
            .db
            .get_clip_for_owner(&user.user_id, session.clip_id)
            .await?
            .map(|c| enrich_clip_dto(c, &state.config, &headers))
    } else {
        None
    };

    Ok(Json(UploadStatusDto {
        session_id: session.session_id,
        clip_id: session.clip_id,
        status: session.status,
        bytes_received: session.bytes_received,
        total_bytes: session.total_bytes,
        uploaded_parts: session.uploaded_parts,
        total_parts: session.total_parts,
        expires_at: session.expires_at,
        clip,
    }))
}

async fn complete_upload_session(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<UploadStatusDto>, ServerError> {
    let session = state
        .db
        .mark_upload_finalizing(&user.user_id, &session_id)
        .await?;

    let multipart_id = session.multipart_upload_id.clone().ok_or_else(|| {
        ServerError::Conflict("upload session missing multipart upload id".to_string())
    })?;

    let mut parts = {
        let mut guard = state
            .upload_parts
            .lock()
            .map_err(|_| ServerError::Internal("upload parts lock poisoned".to_string()))?;
        guard.remove(&session_id).unwrap_or_default()
    };
    parts.sort_by_key(|p| p.part_number);

    let obj_meta = match state
        .storage
        .complete_multipart(&session.storage_key, &multipart_id, &parts)
        .await
    {
        Ok(meta) => meta,
        Err(err) => {
            let _ = state
                .storage
                .abort_multipart(&session.storage_key, &multipart_id)
                .await;
            let _ = state
                .db
                .fail_upload_session(&user.user_id, &session_id)
                .await;
            return Err(err.into());
        }
    };

    let (completed_session, clip) = match state
        .db
        .complete_upload_session(
            &user.user_id,
            &session_id,
            obj_meta.size_bytes,
            session.sha256.clone(),
        )
        .await
    {
        Ok(res) => res,
        Err(err) => {
            let _ = state.storage.delete_object(&session.storage_key).await;
            let _ = state
                .db
                .fail_upload_session(&user.user_id, &session_id)
                .await;
            return Err(err.into());
        }
    };

    Ok(Json(UploadStatusDto {
        session_id: completed_session.session_id,
        clip_id: completed_session.clip_id,
        status: completed_session.status,
        bytes_received: completed_session.bytes_received,
        total_bytes: completed_session.total_bytes,
        uploaded_parts: completed_session.uploaded_parts,
        total_parts: completed_session.total_parts,
        expires_at: completed_session.expires_at,
        clip: Some(enrich_clip_dto(clip, &state.config, &headers)),
    }))
}

async fn abort_upload_session(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ServerError> {
    let Some(aborted) = state
        .db
        .abort_upload_session(&user.user_id, &session_id)
        .await?
    else {
        return Err(ServerError::NotFound(
            "upload session not found".to_string(),
        ));
    };

    if let Ok(mut guard) = state.upload_parts.lock() {
        guard.remove(&session_id);
    }

    if let Some(mp_id) = aborted.multipart_upload_id.as_deref() {
        let _ = state
            .storage
            .abort_multipart(&aborted.storage_key, mp_id)
            .await;
    }

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Direct Multipart Upload & Clip CRUD
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DirectUploadMetadata {
    #[serde(default)]
    id: Option<Uuid>,
    title: String,
    #[serde(default)]
    game: Option<GameInfo>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    fps: f32,
    #[serde(default)]
    codec: String,
    #[serde(default)]
    audio_tracks: Vec<AudioTrackInfo>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
}

async fn upload_clip_multipart(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ClipDto>), ServerError> {
    let mut metadata_opt: Option<DirectUploadMetadata> = None;
    let mut visibility_opt: Option<ShareVisibility> = None;
    let mut temp_file: Option<tempfile::NamedTempFile> = None;
    let mut uploaded_size: u64 = 0;
    let mut original_filename: Option<String> = None;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ServerError::BadRequest(format!("invalid multipart payload: {e}")))?
    {
        let field_name = field.name().unwrap_or_default().to_string();
        match field_name.as_str() {
            "metadata" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| ServerError::BadRequest(format!("invalid metadata field: {e}")))?;
                let meta: DirectUploadMetadata = serde_json::from_str(&text)
                    .map_err(|e| ServerError::BadRequest(format!("invalid metadata json: {e}")))?;
                metadata_opt = Some(meta);
            }
            "visibility" => {
                let text = field.text().await.map_err(|e| {
                    ServerError::BadRequest(format!("invalid visibility field: {e}"))
                })?;
                visibility_opt = match text.trim().to_ascii_lowercase().as_str() {
                    "public" => Some(ShareVisibility::Public),
                    "private" => Some(ShareVisibility::Private),
                    _ => {
                        return Err(ServerError::BadRequest(
                            "visibility must be 'public' or 'private'".to_string(),
                        ));
                    }
                };
            }
            "file" => {
                original_filename = field.file_name().map(ToString::to_string);
                let named = tempfile::NamedTempFile::new()?;
                let std_file = named.reopen()?;
                let mut async_file = tokio::fs::File::from_std(std_file);
                let mut total: u64 = 0;

                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|e| ServerError::BadRequest(format!("upload stream error: {e}")))?
                {
                    total = total.saturating_add(chunk.len() as u64);
                    if total > state.config.max_upload_bytes {
                        return Err(ServerError::PayloadTooLarge(format!(
                            "upload exceeds max size of {} bytes",
                            state.config.max_upload_bytes
                        )));
                    }
                    async_file.write_all(&chunk).await?;
                }
                async_file.flush().await?;
                uploaded_size = total;
                temp_file = Some(named);
            }
            _ => {}
        }
    }

    let named_temp = temp_file
        .ok_or_else(|| ServerError::BadRequest("missing 'file' multipart field".to_string()))?;
    if uploaded_size == 0 {
        return Err(ServerError::BadRequest(
            "uploaded file is empty".to_string(),
        ));
    }
    let meta = metadata_opt
        .ok_or_else(|| ServerError::BadRequest("missing 'metadata' multipart field".to_string()))?;

    let clip_id = meta.id.unwrap_or_else(Uuid::new_v4);
    let clip_id_str = clip_id.to_string();
    let storage_key = build_clip_object_key(&user.user_id, &clip_id_str, ClipObjectKind::Original)
        .map_err(|e| ServerError::BadRequest(e.to_string()))?;
    let content_type =
        content_type_for_filename(original_filename.as_deref().unwrap_or("clip.mp4")).to_string();

    state
        .storage
        .put_file(
            &storage_key,
            named_temp.path(),
            uploaded_size,
            &content_type,
        )
        .await?;

    let now = Utc::now();
    let new_clip = NewClip {
        id: clip_id,
        owner_id: user.user_id.clone(),
        title: meta.title,
        game: meta.game,
        duration_ms: meta.duration_ms,
        width: meta.width,
        height: meta.height,
        fps: meta.fps,
        size_bytes: uploaded_size,
        codec: meta.codec,
        content_type: content_type.clone(),
        audio_tracks: meta.audio_tracks,
        tags: meta.tags,
        notes: meta.notes,
        visibility: visibility_opt.unwrap_or(ShareVisibility::Private),
        created_at: meta.created_at.unwrap_or(now),
        uploaded_at: now,
        expires_at: None,
    };
    let asset = NewClipAsset {
        id: Uuid::new_v4().to_string(),
        clip_id,
        owner_id: user.user_id.clone(),
        asset_kind: ClipAssetKind::Original,
        storage_key: storage_key.clone(),
        content_type,
        size_bytes: uploaded_size,
        sha256: None,
    };

    let created = match state
        .db
        .create_clip_with_assets(new_clip, vec![asset])
        .await
    {
        Ok(c) => c,
        Err(err) => {
            let _ = state.storage.delete_object(&storage_key).await;
            return Err(err.into());
        }
    };

    Ok((
        StatusCode::CREATED,
        Json(enrich_clip_dto(created, &state.config, &headers)),
    ))
}

async fn list_clips(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Query(q): Query<PaginationQuery>,
) -> Result<Json<Vec<ClipDto>>, ServerError> {
    let clips = state
        .db
        .list_clips_for_owner(&user.user_id, q.limit.unwrap_or(50), q.offset.unwrap_or(0))
        .await?;
    let resp = clips
        .into_iter()
        .map(|c| enrich_clip_dto(c, &state.config, &headers))
        .collect();
    Ok(Json(resp))
}

async fn get_clip(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ClipDto>, ServerError> {
    let clip = state
        .db
        .get_clip_for_owner(&user.user_id, id)
        .await?
        .ok_or_else(|| ServerError::NotFound("clip not found".to_string()))?;
    Ok(Json(enrich_clip_dto(clip, &state.config, &headers)))
}

async fn update_clip(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateClipRequest>,
) -> Result<Json<ClipDto>, ServerError> {
    let updated = state
        .db
        .update_clip_for_owner(&user.user_id, id, req)
        .await?
        .ok_or_else(|| ServerError::NotFound("clip not found".to_string()))?;
    Ok(Json(enrich_clip_dto(updated, &state.config, &headers)))
}

async fn delete_clip(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ServerError> {
    if state
        .db
        .get_clip_for_owner(&user.user_id, id)
        .await?
        .is_none()
    {
        return Err(ServerError::NotFound("clip not found".to_string()));
    }

    let assets = state
        .db
        .list_clip_assets_for_owner(&user.user_id, id)
        .await?;

    // Purge object storage before removing DB metadata so partial/transient
    // storage failures leave the clip and assets recoverable on retry.
    for asset in &assets {
        if let Err(err) = state.storage.delete_object(&asset.storage_key).await {
            tracing::error!(
                clip_id = %id,
                storage_key = %asset.storage_key,
                error = %err,
                "failed to delete clip storage object; preserving database record for retry"
            );
            return Err(err.into());
        }
    }

    state
        .db
        .delete_clip_for_owner(&user.user_id, id)
        .await?
        .ok_or_else(|| ServerError::NotFound("clip not found".to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

async fn get_clip_thumbnail(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Response, ServerError> {
    let asset = state
        .db
        .get_clip_asset_for_owner(&user.user_id, id, ClipAssetKind::Thumbnail)
        .await?
        .ok_or_else(|| ServerError::NotFound("thumbnail not found".to_string()))?;

    let meta = state.storage.head_object(&asset.storage_key).await?;
    let range_obj = state
        .storage
        .get_object_range(&asset.storage_key, 0, None)
        .await?;

    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                meta.content_type.unwrap_or(asset.content_type),
            ),
            (header::CONTENT_LENGTH, meta.size_bytes.to_string()),
        ],
        Body::from_stream(range_obj.stream),
    )
        .into_response())
}

async fn head_clip_video(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Response, ServerError> {
    let asset = state
        .db
        .get_clip_asset_for_owner(&user.user_id, id, ClipAssetKind::Original)
        .await?
        .ok_or_else(|| ServerError::NotFound("clip video not found".to_string()))?;

    head_object_response(
        state.storage.as_ref(),
        &asset.storage_key,
        &asset.content_type,
    )
    .await
}

async fn stream_clip_video(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, ServerError> {
    let asset = state
        .db
        .get_clip_asset_for_owner(&user.user_id, id, ClipAssetKind::Original)
        .await?
        .ok_or_else(|| ServerError::NotFound("clip video not found".to_string()))?;

    stream_object_response(
        state.storage.as_ref(),
        &asset.storage_key,
        &asset.content_type,
        &headers,
    )
    .await
}

// ---------------------------------------------------------------------------
// Shares
// ---------------------------------------------------------------------------

async fn create_clip_share(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(clip_id): Path<Uuid>,
    body: Option<Json<CreateShareRequest>>,
) -> Result<(StatusCode, Json<CreateShareResponse>), ServerError> {
    let req = body.map(|b| b.0).unwrap_or_default();
    let share_id = generate_share_token()?;
    let expires_at = req
        .expires_in_hours
        .map(|h| Utc::now() + Duration::hours(i64::try_from(h).unwrap_or(24 * 365)));

    let record = state
        .db
        .create_share(&user.user_id, clip_id, share_id, req.visibility, expires_at)
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateShareResponse {
            url: build_share_url(&state.config, &headers, &record.share_id),
            share_id: record.share_id,
            clip_id: record.clip_id,
            visibility: record.visibility,
            expires_at: record.expires_at,
            created_at: record.created_at,
        }),
    ))
}

#[derive(Debug, Deserialize)]
struct ListSharesQuery {
    clip_id: Option<Uuid>,
}

async fn list_user_shares(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(q): Query<ListSharesQuery>,
) -> Result<Json<Vec<ShareRecord>>, ServerError> {
    if let Some(clip_id) = q.clip_id {
        let shares = state
            .db
            .list_shares_for_clip(&user.user_id, clip_id)
            .await?;
        return Ok(Json(shares));
    }
    let clips = state.db.list_clips_for_owner(&user.user_id, 200, 0).await?;
    let mut all = Vec::new();
    for clip in clips {
        let mut shares = state
            .db
            .list_shares_for_clip(&user.user_id, clip.id)
            .await?;
        all.append(&mut shares);
    }
    Ok(Json(all))
}

#[derive(Debug, Deserialize)]
struct UpdateShareVisibilityRequest {
    visibility: ShareVisibility,
}

async fn update_share_visibility(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(share_id): Path<String>,
    Json(req): Json<UpdateShareVisibilityRequest>,
) -> Result<Json<CreateShareResponse>, ServerError> {
    let record = state
        .db
        .update_share_visibility(&user.user_id, &share_id, req.visibility)
        .await?
        .ok_or_else(|| ServerError::NotFound("share not found".to_string()))?;

    Ok(Json(CreateShareResponse {
        url: build_share_url(&state.config, &headers, &record.share_id),
        share_id: record.share_id,
        clip_id: record.clip_id,
        visibility: record.visibility,
        expires_at: record.expires_at,
        created_at: record.created_at,
    }))
}

async fn revoke_share(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(share_id): Path<String>,
) -> Result<StatusCode, ServerError> {
    if state.db.revoke_share(&user.user_id, &share_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ServerError::NotFound("share not found".to_string()))
    }
}

async fn resolve_share_lookup(
    state: &AppState,
    token: &str,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<clipforge_db::PublicShareLookup, ServerError> {
    match state.db.lookup_public_share(token, Utc::now()).await? {
        ShareLookupOutcome::Active(lookup) => {
            if lookup.clip.visibility == ShareVisibility::Private {
                let Some(user) = try_authenticate(headers, query_token, state).await? else {
                    return Err(ServerError::Unauthorized(
                        "authentication required for private clip".to_string(),
                    ));
                };
                if user.user_id != lookup.share.owner_id {
                    return Err(ServerError::Forbidden(
                        "you do not have access to this private clip".to_string(),
                    ));
                }
            }
            Ok(*lookup)
        }
        ShareLookupOutcome::Revoked => Err(ServerError::Gone {
            code: "share_revoked",
            message: "this share link has been revoked".to_string(),
        }),
        ShareLookupOutcome::Expired => Err(ServerError::Gone {
            code: "share_expired",
            message: "this share link has expired".to_string(),
        }),
        ShareLookupOutcome::Private => {
            Err(ServerError::Forbidden("this share is private".to_string()))
        }
        ShareLookupOutcome::NotFound => Err(ServerError::NotFound("share not found".to_string())),
    }
}

async fn get_public_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Path(token): Path<String>,
) -> Result<Json<PublicShareDto>, ServerError> {
    let query_tok = params.get("token").map(String::as_str);
    let lookup = resolve_share_lookup(&state, &token, &headers, query_tok).await?;

    let base = resolve_base_url(&state.config, &headers);
    let url = build_share_url(&state.config, &headers, &lookup.share.share_id);
    let video_url = if base.is_empty() {
        format!("/api/v1/shares/{}/video", lookup.share.share_id)
    } else {
        format!("{base}/api/v1/shares/{}/video", lookup.share.share_id)
    };

    Ok(Json(PublicShareDto {
        share_id: lookup.share.share_id,
        clip_id: lookup.clip.id,
        title: lookup.clip.title,
        game: lookup.clip.game,
        duration_ms: lookup.clip.duration_ms,
        width: lookup.clip.width,
        height: lookup.clip.height,
        fps: lookup.clip.fps,
        size_bytes: lookup.clip.size_bytes,
        codec: lookup.clip.codec,
        content_type: lookup.clip.content_type,
        tags: lookup.clip.tags,
        notes: lookup.clip.notes,
        visibility: lookup.share.visibility,
        url,
        video_url,
        thumbnail_url: None,
        created_at: lookup.clip.created_at,
        uploaded_at: lookup.clip.uploaded_at,
        expires_at: lookup.share.expires_at,
    }))
}

async fn head_share_video(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Path(token): Path<String>,
) -> Result<Response, ServerError> {
    let query_tok = params.get("token").map(String::as_str);
    let lookup = resolve_share_lookup(&state, &token, &headers, query_tok).await?;
    let asset = lookup
        .original_asset
        .ok_or_else(|| ServerError::NotFound("clip video asset not found".to_string()))?;

    head_object_response(
        state.storage.as_ref(),
        &asset.storage_key,
        &asset.content_type,
    )
    .await
}

async fn stream_share_video(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    Path(token): Path<String>,
) -> Result<Response, ServerError> {
    let query_tok = params.get("token").map(String::as_str);
    let lookup = resolve_share_lookup(&state, &token, &headers, query_tok).await?;
    let asset = lookup
        .original_asset
        .ok_or_else(|| ServerError::NotFound("clip video asset not found".to_string()))?;

    stream_object_response(
        state.storage.as_ref(),
        &asset.storage_key,
        &asset.content_type,
        &headers,
    )
    .await
}

// ---------------------------------------------------------------------------
// Admin Endpoints
// ---------------------------------------------------------------------------

async fn admin_list_users(
    State(state): State<AppState>,
    caller: AuthenticatedUser,
    Query(q): Query<PaginationQuery>,
) -> Result<Json<Vec<UserInfo>>, ServerError> {
    require_admin(&caller)?;
    let users = state
        .db
        .list_users(q.limit.unwrap_or(100), q.offset.unwrap_or(0))
        .await?;
    let mut out = Vec::with_capacity(users.len());
    for u in users {
        let quota = state.db.get_storage_usage(&u.id).await.ok();
        out.push(u.to_user_info(quota));
    }
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
struct AdminCreateUserRequest {
    username: String,
    password: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    role: UserRole,
    #[serde(default)]
    quota_limit_bytes: Option<u64>,
}

async fn admin_create_user(
    State(state): State<AppState>,
    caller: AuthenticatedUser,
    Json(req): Json<AdminCreateUserRequest>,
) -> Result<(StatusCode, Json<UserInfo>), ServerError> {
    require_admin(&caller)?;
    let user = create_user(
        &state.db,
        &req.username,
        &req.password,
        req.display_name,
        req.role,
        req.quota_limit_bytes
            .unwrap_or(state.config.default_quota_bytes),
    )
    .await?;
    let quota = state.db.get_storage_usage(&user.id).await.ok();
    Ok((StatusCode::CREATED, Json(user.to_user_info(quota))))
}

#[derive(Debug, Deserialize)]
struct AdminUpdateQuotaRequest {
    quota_limit_bytes: u64,
}

async fn admin_update_quota(
    State(state): State<AppState>,
    caller: AuthenticatedUser,
    Path(user_id): Path<String>,
    Json(req): Json<AdminUpdateQuotaRequest>,
) -> Result<Json<UserInfo>, ServerError> {
    require_admin(&caller)?;
    state
        .db
        .update_user_quota_limit(&user_id, req.quota_limit_bytes)
        .await?;
    let user = state
        .db
        .get_user_by_id(&user_id)
        .await?
        .ok_or_else(|| ServerError::NotFound("user not found".to_string()))?;
    let quota = state.db.get_storage_usage(&user_id).await.ok();
    Ok(Json(user.to_user_info(quota)))
}

fn content_type_for_filename(filename: &str) -> &'static str {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("mp4") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("flv") => "video/x-flv",
        _ => "video/mp4",
    }
}
