//! ClipForge Server HTTP router, configuration, and application state.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::{Json, Router, extract::State, routing::get};
use clipforge_api::{HealthDto, OPENAPI_SPEC_YAML, ServerInfoDto};
use clipforge_db::Database;
use clipforge_storage::StorageConfig;
use tower_http::trace::TraceLayer;

/// Runtime configuration for `clipforge-server`.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub database_path: PathBuf,
    pub storage: StorageConfig,
    pub max_upload_bytes: u64,
    pub chunk_size_bytes: u64,
    pub public_base_url: Option<String>,
    pub registration_enabled: bool,
    pub session_ttl_hours: i64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:3000".to_string(),
            database_path: PathBuf::from("clipforge-server.db"),
            storage: StorageConfig::Local {
                root_dir: PathBuf::from("data/clips"),
            },
            max_upload_bytes: 500 * 1024 * 1024,
            chunk_size_bytes: 8 * 1024 * 1024,
            public_base_url: None,
            registration_enabled: true,
            session_ttl_hours: 24 * 30,
        }
    }
}

/// Shared application state for Axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub db: Database,
    pub started_at: Instant,
}

/// Builds the Axum [`Router`] for `clipforge-server`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/health", get(health))
        .route("/api/v1/server/info", get(server_info))
        .route("/api/v1/openapi.yaml", get(openapi_spec))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<HealthDto> {
    let db_ok = state.db.health_check().await.is_ok();
    Json(HealthDto {
        status: if db_ok { "ok" } else { "degraded" }.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        db_ok,
        storage_ok: true,
        uptime_secs: state.started_at.elapsed().as_secs(),
    })
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

async fn openapi_spec() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "application/yaml; charset=utf-8")],
        OPENAPI_SPEC_YAML,
    )
}
