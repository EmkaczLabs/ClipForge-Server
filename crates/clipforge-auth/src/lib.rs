//! Authentication, password hashing, and token verification for ClipForge Server.
//!
//! Owned by `@srv-db-auth`. Uses Argon2id (`argon2`) for passwords and
//! SHA-256 token hashing with constant-time comparison (`subtle`).

use std::fmt;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{Duration, Utc};
use clipforge_api::{ApiTokenInfo, CreateTokenRequest, CreateTokenResponse, UserRole};
use clipforge_db::{Database, NewApiToken, NewUser, UserRecord};
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;

/// Prefix for generated personal API tokens.
pub const API_TOKEN_PREFIX: &str = "cf_pat_";

/// Minimum password length enforced during user registration/bootstrap.
pub const MIN_PASSWORD_LEN: usize = 8;

/// Errors returned by authentication and authorization helpers.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("token expired")]
    TokenExpired,
    #[error("token revoked")]
    TokenRevoked,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("validation error: {0}")]
    Validation(String),
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
    pub role: UserRole,
    pub scopes: Vec<String>,
}

/// Generated raw personal API token along with its metadata and SHA-256 hash.
///
/// `Debug` redacts the raw token and hash so secrets are never logged.
#[derive(Clone)]
pub struct GeneratedToken {
    pub id: String,
    pub prefix: String,
    pub raw_token: String,
    pub token_hash: String,
}

impl fmt::Debug for GeneratedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedToken")
            .field("id", &self.id)
            .field("prefix", &self.prefix)
            .field("raw_token", &"[REDACTED]")
            .field("token_hash", &"[REDACTED]")
            .finish()
    }
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

fn argon2_instance() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default())
}

/// Validates username format for account creation.
pub fn validate_username(username: &str) -> Result<String> {
    let trimmed = username.trim();
    if trimmed.len() < 2 || trimmed.len() > 64 {
        return Err(AuthError::Validation(
            "username must be between 2 and 64 characters".into(),
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(AuthError::Validation(
            "username may only contain ASCII letters, digits, '_', '-', or '.'".into(),
        ));
    }
    Ok(trimmed.to_string())
}

/// Validates password minimum length.
pub fn validate_password(password: &str) -> Result<()> {
    if password.len() < MIN_PASSWORD_LEN {
        return Err(AuthError::Validation(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if password.len() > 1024 {
        return Err(AuthError::Validation(
            "password exceeds maximum length".into(),
        ));
    }
    Ok(())
}

/// Hashes a password using Argon2id with a fresh 16-byte CSPRNG salt.
pub fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() {
        return Err(AuthError::Validation("password cannot be empty".into()));
    }
    let rng = SystemRandom::new();
    let mut salt_bytes = [0u8; 16];
    rng.fill(&mut salt_bytes)
        .map_err(|_| AuthError::Crypto("failed to generate random salt".into()))?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| AuthError::Crypto(format!("failed to encode salt: {e}")))?;

    argon2_instance()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Crypto(format!("argon2id hash failed: {e}")))
}

/// Verifies a plaintext password against an Argon2id PHC hash string.
pub fn verify_password(password: &str, password_hash: &str) -> Result<bool> {
    let parsed = PasswordHash::new(password_hash)
        .map_err(|e| AuthError::Crypto(format!("invalid password hash format: {e}")))?;
    Ok(argon2_instance()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// Computes the lowercase hex SHA-256 digest of a raw API token.
pub fn hash_token(raw_token: &str) -> String {
    let sum = digest(&SHA256, raw_token.as_bytes());
    hex::encode(sum.as_ref())
}

/// Constant-time comparison of two SHA-256 token hex digests.
pub fn constant_time_hash_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Generates a cryptographically random personal API token and its SHA-256 hash.
pub fn generate_api_token() -> Result<GeneratedToken> {
    let rng = SystemRandom::new();
    let mut random_bytes = [0u8; 32];
    rng.fill(&mut random_bytes)
        .map_err(|_| AuthError::Crypto("failed to generate random API token".into()))?;

    let raw_token = format!("{API_TOKEN_PREFIX}{}", hex::encode(random_bytes));
    let prefix = raw_token.chars().take(15).collect();
    let token_hash = hash_token(&raw_token);

    Ok(GeneratedToken {
        id: Uuid::new_v4().to_string(),
        prefix,
        raw_token,
        token_hash,
    })
}

/// Generates a cryptographically random URL-safe share token.
pub fn generate_share_token() -> Result<String> {
    let rng = SystemRandom::new();
    let mut random_bytes = [0u8; 16];
    rng.fill(&mut random_bytes)
        .map_err(|_| AuthError::Crypto("failed to generate random share token".into()))?;
    Ok(hex::encode(random_bytes))
}

/// Bootstraps an initial admin account if and only if no users exist yet.
/// Returns `Ok(Some(admin))` if created, or `Ok(None)` if users already exist.
pub async fn bootstrap_admin(
    db: &Database,
    username: &str,
    password: &str,
    quota_limit_bytes: u64,
) -> Result<Option<UserRecord>> {
    if db.count_users().await? > 0 {
        return Ok(None);
    }
    let user = create_user(
        db,
        username,
        password,
        Some("Administrator".to_string()),
        UserRole::Admin,
        quota_limit_bytes,
    )
    .await?;
    Ok(Some(user))
}

/// Creates a new user account with Argon2id password hashing.
pub async fn create_user(
    db: &Database,
    username: &str,
    password: &str,
    display_name: Option<String>,
    role: UserRole,
    quota_limit_bytes: u64,
) -> Result<UserRecord> {
    let clean_username = validate_username(username)?;
    validate_password(password)?;
    let password_hash = hash_password(password)?;
    let user = db
        .create_user(NewUser {
            id: Uuid::new_v4().to_string(),
            username: clean_username,
            display_name,
            password_hash,
            role,
            quota_limit_bytes,
        })
        .await?;
    Ok(user)
}

/// Authenticates a user by username and password using Argon2id.
pub async fn authenticate_password(
    db: &Database,
    username: &str,
    password: &str,
) -> Result<UserRecord> {
    let Some(user) = db.get_user_by_username(username.trim()).await? else {
        // Perform dummy hash verification to reduce user-enumeration timing variance.
        let _ = hash_password("clipforge-dummy-timing-pad");
        return Err(AuthError::InvalidCredentials);
    };
    if !user.is_active {
        return Err(AuthError::Forbidden("account is disabled".into()));
    }
    if !verify_password(password, &user.password_hash)? {
        return Err(AuthError::InvalidCredentials);
    }
    Ok(user)
}

/// Issues and persists a new personal API token for `user_id`.
///
/// The plaintext token is returned once in [`CreateTokenResponse`] and only its
/// SHA-256 hash is stored in the database.
pub async fn issue_api_token(
    db: &Database,
    user_id: &str,
    req: CreateTokenRequest,
) -> Result<CreateTokenResponse> {
    let name = req.name.trim().to_string();
    if name.is_empty() || name.len() > 100 {
        return Err(AuthError::Validation(
            "token name must be between 1 and 100 characters".into(),
        ));
    }
    let generated = generate_api_token()?;
    let expires_at = req
        .expires_in_days
        .map(|days| Utc::now() + Duration::days(i64::from(days.max(1))));

    let info = db
        .create_api_token(NewApiToken {
            id: generated.id,
            user_id: user_id.to_string(),
            name,
            prefix: generated.prefix,
            token_hash: generated.token_hash,
            scopes: req.scopes,
            expires_at,
        })
        .await?;

    Ok(CreateTokenResponse {
        id: info.id,
        name: info.name,
        prefix: info.prefix,
        token: generated.raw_token,
        scopes: info.scopes,
        created_at: info.created_at,
        expires_at: info.expires_at,
    })
}

/// Verifies a raw API token against the database, enforcing active status,
/// non-revocation, and expiration.
pub async fn verify_api_token(db: &Database, raw_token: &str) -> Result<AuthenticatedUser> {
    let trimmed = raw_token.trim();
    if trimmed.is_empty() {
        return Err(AuthError::InvalidCredentials);
    }
    let token_hash = hash_token(trimmed);
    let Some(record) = db.find_api_token_by_hash(&token_hash).await? else {
        return Err(AuthError::InvalidCredentials);
    };

    if record.revoked_at.is_some() {
        return Err(AuthError::TokenRevoked);
    }
    if let Some(expires_at) = record.token.expires_at
        && expires_at <= Utc::now()
    {
        return Err(AuthError::TokenExpired);
    }
    if !record.is_active {
        return Err(AuthError::Forbidden("account is disabled".into()));
    }

    let _ = db
        .touch_api_token_last_used(&record.token.id, Utc::now())
        .await;

    Ok(AuthenticatedUser {
        user_id: record.user_id,
        username: record.username,
        role: record.role,
        scopes: record.token.scopes,
    })
}

/// Lists non-revoked API tokens belonging strictly to `caller.user_id`.
pub async fn list_my_tokens(
    db: &Database,
    caller: &AuthenticatedUser,
) -> Result<Vec<ApiTokenInfo>> {
    Ok(db.list_api_tokens_for_user(&caller.user_id).await?)
}

/// Revokes a token strictly owned by `caller.user_id`.
pub async fn revoke_my_token(
    db: &Database,
    caller: &AuthenticatedUser,
    token_id: &str,
) -> Result<bool> {
    Ok(db.revoke_api_token(&caller.user_id, token_id).await?)
}

/// Enforces that the authenticated user has [`UserRole::Admin`].
pub fn require_admin(user: &AuthenticatedUser) -> Result<()> {
    if user.role != UserRole::Admin {
        return Err(AuthError::Forbidden("admin role required".into()));
    }
    Ok(())
}

/// Enforces that the authenticated user's token includes `required_scope`
/// (empty scopes or `"*"` grant full access).
pub fn require_scope(user: &AuthenticatedUser, required_scope: &str) -> Result<()> {
    if user.scopes.is_empty() || user.scopes.iter().any(|s| s == "*" || s == required_scope) {
        return Ok(());
    }
    Err(AuthError::Forbidden(format!(
        "missing required scope '{required_scope}'"
    )))
}

/// Enforces strict resource ownership: User A can never access User B's private resources.
pub fn require_owner(user: &AuthenticatedUser, resource_owner_id: &str) -> Result<()> {
    if user.user_id != resource_owner_id {
        return Err(AuthError::Forbidden(
            "resource belongs to another user".into(),
        ));
    }
    Ok(())
}
