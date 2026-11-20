//! ClipForge Server API v1 data transfer objects (DTOs) and OpenAPI 3.1 spec.
//!
//! This crate defines the wire contract shared between `clipforge-server` and
//! external clients (such as `clipforge-cloud-client` in the ClipForge desktop app).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Embedded OpenAPI 3.1.0 specification for ClipForge Server API v1.
pub const OPENAPI_SPEC_YAML: &str = include_str!("../openapi.yaml");

/// Current API version string.
pub const API_VERSION: &str = "v1";

// ---------------------------------------------------------------------------
// Common / Media Metadata Types
// ---------------------------------------------------------------------------

/// Visibility level for a clip or share link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShareVisibility {
    #[default]
    Public,
    Private,
}

/// User role on the ClipForge server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    #[default]
    User,
    Admin,
}

/// Metadata describing the game or application associated with a clip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
}

/// Information about a single audio track inside a media container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioTrackInfo {
    pub index: u32,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Standard pagination query parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaginationQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

// ---------------------------------------------------------------------------
// Errors & Health / Server Info
// ---------------------------------------------------------------------------

/// Structured error payload returned by all `/api/v1` endpoints on failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorDto {
    /// Human-readable error message (compatible with legacy `{ "error": "..." }`).
    pub error: String,
    /// Machine-readable error code (e.g. `"unauthorized"`, `"not_found"`, `"quota_exceeded"`).
    pub code: String,
    /// HTTP status code.
    pub status: u16,
    /// Optional structured error details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiErrorDto {
    /// Creates a new [`ApiErrorDto`].
    pub fn new(status: u16, code: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            code: code.into(),
            status,
            details: None,
        }
    }

    /// Attaches optional structured details to this error.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// Health check response returned by `GET /health` and `GET /api/v1/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthDto {
    pub status: String,
    pub version: String,
    pub db_ok: bool,
    pub storage_ok: bool,
    pub uptime_secs: u64,
}

/// Storage quota usage and limits for a user account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaDto {
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub remaining_bytes: u64,
    pub clip_count: u64,
}

/// Public server capabilities and configuration summary (`GET /api/v1/server/info`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfoDto {
    pub name: String,
    pub version: String,
    pub api_version: String,
    pub storage_backend: String,
    pub max_upload_bytes: u64,
    pub chunk_size_bytes: u64,
    pub registration_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Auth & API Tokens
// ---------------------------------------------------------------------------

/// Request body for `POST /api/v1/auth/register`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Request body for `POST /api/v1/auth/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthLoginRequest {
    pub username: String,
    pub password: String,
}

/// Authenticated user profile (`GET /api/v1/auth/me`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub role: UserRole,
    #[serde(default = "default_true")]
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaDto>,
}

fn default_true() -> bool {
    true
}

/// Response returned by `POST /api/v1/auth/register` and `POST /api/v1/auth/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthLoginResponse {
    pub user_id: String,
    pub username: String,
    pub token: String,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserInfo>,
}

/// Summary of an existing API token (`GET /api/v1/auth/tokens`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiTokenInfo {
    pub id: String,
    pub name: String,
    pub prefix: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Request body for `POST /api/v1/auth/tokens`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_days: Option<u32>,
}

/// Response returned when creating an API token (`POST /api/v1/auth/tokens`).
///
/// The plaintext `token` is only returned once upon creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTokenResponse {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub token: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Clips
// ---------------------------------------------------------------------------

/// Full representation of a clip stored on the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipDto {
    pub id: Uuid,
    pub owner_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameInfo>,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub size_bytes: u64,
    pub codec: String,
    pub content_type: String,
    #[serde(default)]
    pub audio_tracks: Vec<AudioTrackInfo>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub visibility: ShareVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub uploaded_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Request body for `PATCH /api/v1/clips/{id}` to update clip metadata or visibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UpdateClipRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<ShareVisibility>,
}

// ---------------------------------------------------------------------------
// Chunked / Streaming Upload Sessions
// ---------------------------------------------------------------------------

/// Lifecycle status of a streaming/chunked upload session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UploadSessionStatus {
    #[default]
    Pending,
    Uploading,
    Finalizing,
    Completed,
    Failed,
    Aborted,
}

/// Request body for `POST /api/v1/uploads` to initiate a streaming/chunked upload session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateUploadSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_id: Option<Uuid>,
    pub title: String,
    pub filename: String,
    pub total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default)]
    pub visibility: ShareVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameInfo>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub fps: f32,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub audio_tracks: Vec<AudioTrackInfo>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}

/// Response returned by `POST /api/v1/uploads`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateUploadSessionResponse {
    pub session_id: String,
    pub clip_id: Uuid,
    pub upload_url: String,
    pub part_size_bytes: u64,
    pub total_bytes: u64,
    pub total_parts: u32,
    pub expires_at: DateTime<Utc>,
}

/// Status of an upload session (`GET /api/v1/uploads/{session_id}` or `POST .../complete`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadStatusDto {
    pub session_id: String,
    pub clip_id: Uuid,
    pub status: UploadSessionStatus,
    pub bytes_received: u64,
    pub total_bytes: u64,
    pub uploaded_parts: Vec<u32>,
    pub total_parts: u32,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<ClipDto>,
}

// ---------------------------------------------------------------------------
// Shares
// ---------------------------------------------------------------------------

/// Request body for `POST /api/v1/clips/{id}/shares`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CreateShareRequest {
    #[serde(default)]
    pub visibility: ShareVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_hours: Option<u64>,
}

/// Response returned when creating or updating a share link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateShareResponse {
    pub share_id: String,
    pub clip_id: Uuid,
    pub url: String,
    pub visibility: ShareVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Publicly accessible share metadata (`GET /api/v1/shares/{token}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicShareDto {
    pub share_id: String,
    pub clip_id: Uuid,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<GameInfo>,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub size_bytes: u64,
    pub codec: String,
    pub content_type: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub visibility: ShareVisibility,
    pub url: String,
    pub video_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub uploaded_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_spec_is_embedded_and_v3_1() {
        assert!(OPENAPI_SPEC_YAML.contains("openapi: 3.1.0"));
        assert!(OPENAPI_SPEC_YAML.contains("/api/v1/auth/login"));
        assert!(OPENAPI_SPEC_YAML.contains("/api/v1/uploads"));
    }

    #[test]
    fn dto_roundtrip_smoke() {
        let err = ApiErrorDto::new(404, "not_found", "clip not found");
        let json = serde_json::to_string(&err).expect("serialize");
        let decoded: ApiErrorDto = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(err, decoded);
    }
}
