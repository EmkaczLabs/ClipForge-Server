//! Database record models and repository input/output types.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use clipforge_api::{
    ApiTokenInfo, AudioTrackInfo, ClipDto, GameInfo, ShareVisibility, UploadSessionStatus,
    UserInfo, UserRole,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::DbError;

/// Internal database representation of a user account.
#[derive(Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub password_hash: String,
    pub role: UserRole,
    pub is_active: bool,
    pub quota_limit_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl fmt::Debug for UserRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserRecord")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("display_name", &self.display_name)
            .field("password_hash", &"[REDACTED]")
            .field("role", &self.role)
            .field("is_active", &self.is_active)
            .field("quota_limit_bytes", &self.quota_limit_bytes)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl UserRecord {
    /// Converts this user record into a public [`UserInfo`] DTO.
    pub fn to_user_info(&self, quota: Option<clipforge_api::QuotaDto>) -> UserInfo {
        UserInfo {
            id: self.id.clone(),
            username: self.username.clone(),
            display_name: self.display_name.clone(),
            role: self.role,
            is_active: self.is_active,
            created_at: self.created_at,
            quota,
        }
    }
}

/// Parameters for creating a new user row.
#[derive(Clone)]
pub struct NewUser {
    pub id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub password_hash: String,
    pub role: UserRole,
    pub quota_limit_bytes: u64,
}

impl fmt::Debug for NewUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewUser")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("display_name", &self.display_name)
            .field("password_hash", &"[REDACTED]")
            .field("role", &self.role)
            .field("quota_limit_bytes", &self.quota_limit_bytes)
            .finish()
    }
}

/// Parameters for inserting a hashed API token.
#[derive(Clone)]
pub struct NewApiToken {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub prefix: String,
    pub token_hash: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl fmt::Debug for NewApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewApiToken")
            .field("id", &self.id)
            .field("user_id", &self.user_id)
            .field("name", &self.name)
            .field("prefix", &self.prefix)
            .field("token_hash", &"[REDACTED]")
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// API token row joined with its owning user's identity and status.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiTokenWithUser {
    pub token: ApiTokenInfo,
    pub user_id: String,
    pub username: String,
    pub role: UserRole,
    pub is_active: bool,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl fmt::Debug for ApiTokenWithUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiTokenWithUser")
            .field("token_id", &self.token.id)
            .field("user_id", &self.user_id)
            .field("username", &self.username)
            .field("role", &self.role)
            .field("is_active", &self.is_active)
            .field("revoked_at", &self.revoked_at)
            .finish()
    }
}

/// Kind of stored media asset belonging to a clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipAssetKind {
    Original,
    Thumbnail,
    Preview,
}

impl ClipAssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Thumbnail => "thumbnail",
            Self::Preview => "preview",
        }
    }
}

impl FromStr for ClipAssetKind {
    type Err = DbError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "original" => Ok(Self::Original),
            "thumbnail" => Ok(Self::Thumbnail),
            "preview" => Ok(Self::Preview),
            other => Err(DbError::Internal(format!("unknown asset kind: {other}"))),
        }
    }
}

/// Persisted clip asset record (`clip_assets` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipAssetRecord {
    pub id: String,
    pub clip_id: Uuid,
    pub owner_id: String,
    pub asset_kind: ClipAssetKind,
    pub storage_key: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Parameters for creating a clip row.
#[derive(Debug, Clone)]
pub struct NewClip {
    pub id: Uuid,
    pub owner_id: String,
    pub title: String,
    pub game: Option<GameInfo>,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub size_bytes: u64,
    pub codec: String,
    pub content_type: String,
    pub audio_tracks: Vec<AudioTrackInfo>,
    pub tags: Vec<String>,
    pub notes: Option<String>,
    pub visibility: ShareVisibility,
    pub created_at: DateTime<Utc>,
    pub uploaded_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Parameters for inserting or updating a clip asset row.
#[derive(Debug, Clone)]
pub struct NewClipAsset {
    pub id: String,
    pub clip_id: Uuid,
    pub owner_id: String,
    pub asset_kind: ClipAssetKind,
    pub storage_key: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
}

/// Serializable clip metadata stored alongside an in-flight upload session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UploadMetadata {
    pub game: Option<GameInfo>,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub codec: String,
    pub audio_tracks: Vec<AudioTrackInfo>,
    pub tags: Vec<String>,
    pub notes: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Parameters for creating a new upload session with quota reservation.
#[derive(Debug, Clone)]
pub struct NewUploadSession {
    pub session_id: String,
    pub user_id: String,
    pub clip_id: Uuid,
    pub title: String,
    pub filename: String,
    pub content_type: String,
    pub sha256: Option<String>,
    pub visibility: ShareVisibility,
    pub metadata: UploadMetadata,
    pub storage_key: String,
    pub total_bytes: u64,
    pub part_size_bytes: u64,
    pub total_parts: u32,
    pub expires_at: DateTime<Utc>,
}

/// Persisted upload session record (`uploads` table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadSessionRecord {
    pub session_id: String,
    pub user_id: String,
    pub clip_id: Uuid,
    pub status: UploadSessionStatus,
    pub title: String,
    pub filename: String,
    pub content_type: String,
    pub sha256: Option<String>,
    pub visibility: ShareVisibility,
    pub metadata: UploadMetadata,
    pub storage_key: String,
    pub multipart_upload_id: Option<String>,
    pub total_bytes: u64,
    pub bytes_received: u64,
    pub reserved_bytes: u64,
    pub part_size_bytes: u64,
    pub total_parts: u32,
    pub uploaded_parts: Vec<u32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Persisted share link record (`shares` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRecord {
    pub share_id: String,
    pub clip_id: Uuid,
    pub owner_id: String,
    pub visibility: ShareVisibility,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Resolved public share payload with clip metadata and asset keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicShareLookup {
    pub share: ShareRecord,
    pub clip: ClipDto,
    pub original_asset: Option<ClipAssetRecord>,
    pub thumbnail_asset: Option<ClipAssetRecord>,
}

/// Outcome of resolving a share token for public access.
#[derive(Debug, Clone, PartialEq)]
pub enum ShareLookupOutcome {
    Active(Box<PublicShareLookup>),
    NotFound,
    Revoked,
    Expired,
    Private,
}

pub(crate) fn role_to_str(role: UserRole) -> &'static str {
    match role {
        UserRole::User => "user",
        UserRole::Admin => "admin",
    }
}

pub(crate) fn str_to_role(s: &str) -> Result<UserRole, DbError> {
    match s {
        "user" => Ok(UserRole::User),
        "admin" => Ok(UserRole::Admin),
        other => Err(DbError::Internal(format!("unknown user role: {other}"))),
    }
}

pub(crate) fn visibility_to_str(v: ShareVisibility) -> &'static str {
    match v {
        ShareVisibility::Public => "public",
        ShareVisibility::Private => "private",
    }
}

pub(crate) fn str_to_visibility(s: &str) -> Result<ShareVisibility, DbError> {
    match s {
        "public" => Ok(ShareVisibility::Public),
        "private" => Ok(ShareVisibility::Private),
        other => Err(DbError::Internal(format!("unknown visibility: {other}"))),
    }
}

pub(crate) fn upload_status_to_str(status: UploadSessionStatus) -> &'static str {
    match status {
        UploadSessionStatus::Pending => "pending",
        UploadSessionStatus::Uploading => "uploading",
        UploadSessionStatus::Finalizing => "finalizing",
        UploadSessionStatus::Completed => "completed",
        UploadSessionStatus::Failed => "failed",
        UploadSessionStatus::Aborted => "aborted",
    }
}

pub(crate) fn str_to_upload_status(s: &str) -> Result<UploadSessionStatus, DbError> {
    match s {
        "pending" => Ok(UploadSessionStatus::Pending),
        "uploading" => Ok(UploadSessionStatus::Uploading),
        "finalizing" => Ok(UploadSessionStatus::Finalizing),
        "completed" => Ok(UploadSessionStatus::Completed),
        "failed" => Ok(UploadSessionStatus::Failed),
        "aborted" => Ok(UploadSessionStatus::Aborted),
        other => Err(DbError::Internal(format!("unknown upload status: {other}"))),
    }
}

pub(crate) fn parse_dt(s: &str) -> Result<DateTime<Utc>, DbError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| DbError::Internal(format!("invalid timestamp '{s}': {e}")))
}

pub(crate) fn parse_opt_dt(s: Option<String>) -> Result<Option<DateTime<Utc>>, DbError> {
    s.as_deref().map(parse_dt).transpose()
}
