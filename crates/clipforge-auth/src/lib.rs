//! Authentication, password hashing, and token verification for ClipForge Server.
//!
//! Owned by `@srv-db-auth`. Uses PBKDF2-HMAC-SHA256 for passwords and
//! SHA-256 token hashing with constant-time comparison (`subtle`).

use thiserror::Error;

/// Errors returned by authentication helpers.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("database error: {0}")]
    Db(#[from] clipforge_db::DbError),
}

pub type Result<T> = std::result::Result<T, AuthError>;

/// Authenticated caller identity extracted from a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedUser {
    pub user_id: String,
    pub username: String,
    pub role: clipforge_api::UserRole,
    pub scopes: Vec<String>,
}

/// Extracts a raw bearer token or `x-api-key` credential from request headers.
pub fn extract_raw_credential(
    auth_header: Option<&str>,
    api_key_header: Option<&str>,
    query_token: Option<&str>,
) -> Option<String> {
    if let Some(auth) = auth_header {
        let trimmed = auth.trim();
        if let Some(bearer) = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
        {
            let tok = bearer.trim();
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
        }
    }
    if let Some(key) = api_key_header.map(str::trim).filter(|k| !k.is_empty()) {
        return Some(key.to_string());
    }
    query_token
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(ToString::to_string)
}
