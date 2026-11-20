//! Unified HTTP error mapping to [`clipforge_api::ApiErrorDto`].

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use clipforge_api::ApiErrorDto;
use clipforge_auth::AuthError;
use clipforge_db::DbError;
use clipforge_storage::StorageError;

/// Unified server error mapped to structured JSON HTTP responses.
#[derive(Debug)]
pub enum ServerError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    Gone { code: &'static str, message: String },
    PayloadTooLarge(String),
    QuotaExceeded(String),
    RangeNotSatisfiable { total_bytes: u64 },
    ServiceUnavailable(String),
    Internal(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Self::Unauthorized(msg) => write!(f, "unauthorized: {msg}"),
            Self::Forbidden(msg) => write!(f, "forbidden: {msg}"),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::Conflict(msg) => write!(f, "conflict: {msg}"),
            Self::Gone { message, .. } => write!(f, "gone: {message}"),
            Self::PayloadTooLarge(msg) => write!(f, "payload too large: {msg}"),
            Self::QuotaExceeded(msg) => write!(f, "quota exceeded: {msg}"),
            Self::RangeNotSatisfiable { total_bytes } => {
                write!(f, "range not satisfiable for total_bytes={total_bytes}")
            }
            Self::ServiceUnavailable(msg) => write!(f, "service unavailable: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        match self {
            Self::RangeNotSatisfiable { total_bytes } => {
                let status = StatusCode::RANGE_NOT_SATISFIABLE;
                let body = ApiErrorDto::new(
                    status.as_u16(),
                    "range_not_satisfiable",
                    "range not satisfiable",
                );
                let mut resp = (status, Json(body)).into_response();
                if let Ok(val) = HeaderValue::from_str(&format!("bytes */{total_bytes}")) {
                    resp.headers_mut().insert(header::CONTENT_RANGE, val);
                }
                resp
            }
            other => {
                let (status, code, message) = match other {
                    Self::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m),
                    Self::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "unauthorized", m),
                    Self::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden", m),
                    Self::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m),
                    Self::Conflict(m) => (StatusCode::CONFLICT, "conflict", m),
                    Self::Gone { code, message } => (StatusCode::GONE, code, message),
                    Self::PayloadTooLarge(m) => {
                        (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", m)
                    }
                    Self::QuotaExceeded(m) => (StatusCode::PAYLOAD_TOO_LARGE, "quota_exceeded", m),
                    Self::ServiceUnavailable(m) => {
                        (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable", m)
                    }
                    Self::Internal(m) => {
                        tracing::error!(error = %m, "internal server error");
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "internal_error",
                            "internal server error".to_string(),
                        )
                    }
                    Self::RangeNotSatisfiable { .. } => unreachable!(),
                };
                let body = ApiErrorDto::new(status.as_u16(), code, message);
                (status, Json(body)).into_response()
            }
        }
    }
}

impl From<DbError> for ServerError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound(m) => Self::NotFound(m),
            DbError::Conflict(m) => Self::Conflict(m),
            DbError::InvalidInput(m) => Self::BadRequest(m),
            DbError::QuotaExceeded {
                used_bytes,
                reserved_bytes,
                requested_bytes,
                limit_bytes,
            } => Self::QuotaExceeded(format!(
                "storage quota exceeded (used={used_bytes}, reserved={reserved_bytes}, requested={requested_bytes}, limit={limit_bytes})"
            )),
            DbError::Serialization(e) => Self::Internal(format!("serialization error: {e}")),
            DbError::Sqlite(e) => Self::Internal(format!("database error: {e}")),
            DbError::Migration(e) => Self::Internal(format!("migration error: {e}")),
            DbError::Internal(e) => Self::Internal(e),
        }
    }
}

impl From<AuthError> for ServerError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidCredentials => Self::Unauthorized("invalid credentials".to_string()),
            AuthError::TokenExpired => Self::Unauthorized("token expired".to_string()),
            AuthError::TokenRevoked => Self::Unauthorized("token revoked".to_string()),
            AuthError::Forbidden(m) => Self::Forbidden(m),
            AuthError::Validation(m) => Self::BadRequest(m),
            AuthError::Crypto(m) => Self::Internal(format!("crypto error: {m}")),
            AuthError::Db(e) => Self::from(e),
        }
    }
}

impl From<StorageError> for ServerError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::InvalidKey(m) | StorageError::InvalidRange(m) => Self::BadRequest(m),
            StorageError::NotFound(m) => Self::NotFound(m),
            StorageError::InsufficientSpace { .. } | StorageError::Unavailable(_) => {
                Self::ServiceUnavailable(err.to_string())
            }
            StorageError::Io(e) => Self::Internal(format!("storage i/o error: {e}")),
            StorageError::Http(e) => Self::ServiceUnavailable(format!("storage http error: {e}")),
            StorageError::Backend(m) => Self::Internal(format!("storage backend error: {m}")),
        }
    }
}

impl From<std::io::Error> for ServerError {
    fn from(err: std::io::Error) -> Self {
        Self::Internal(format!("i/o error: {err}"))
    }
}
