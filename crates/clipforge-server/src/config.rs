//! Server runtime configuration loaded from `CLIPFORGE_*` and `S3_*` env vars.

use std::fmt;
use std::path::PathBuf;

use clipforge_storage::{S3Config, StorageConfig};

/// Default per-user storage quota (5 GiB).
pub const DEFAULT_QUOTA_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Runtime configuration for `clipforge-server`.
#[derive(Clone)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub database_path: PathBuf,
    pub storage: StorageConfig,
    pub max_upload_bytes: u64,
    pub chunk_size_bytes: u64,
    pub default_quota_bytes: u64,
    pub public_base_url: Option<String>,
    pub web_dir: Option<PathBuf>,
    pub registration_enabled: bool,
    pub session_ttl_hours: i64,
    pub admin_bootstrap_username: Option<String>,
    pub admin_bootstrap_password: Option<String>,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("bind_addr", &self.bind_addr)
            .field("database_path", &self.database_path)
            .field("storage", &self.storage)
            .field("max_upload_bytes", &self.max_upload_bytes)
            .field("chunk_size_bytes", &self.chunk_size_bytes)
            .field("default_quota_bytes", &self.default_quota_bytes)
            .field("public_base_url", &self.public_base_url)
            .field("web_dir", &self.web_dir)
            .field("registration_enabled", &self.registration_enabled)
            .field("session_ttl_hours", &self.session_ttl_hours)
            .field("admin_bootstrap_username", &self.admin_bootstrap_username)
            .field(
                "admin_bootstrap_password",
                &self.admin_bootstrap_password.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
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
            default_quota_bytes: DEFAULT_QUOTA_BYTES,
            public_base_url: None,
            web_dir: None,
            registration_enabled: true,
            session_ttl_hours: 24 * 30,
            admin_bootstrap_username: None,
            admin_bootstrap_password: None,
        }
    }
}

impl ServerConfig {
    /// Loads configuration from `CLIPFORGE_*` and `S3_*` environment variables.
    pub fn from_env() -> Result<Self, String> {
        let mut cfg = Self::default();

        if let Ok(v) = std::env::var("CLIPFORGE_BIND_ADDR")
            && !v.trim().is_empty()
        {
            cfg.bind_addr = v.trim().to_string();
        }
        if let Ok(v) = std::env::var("CLIPFORGE_DATABASE_PATH")
            && !v.trim().is_empty()
        {
            cfg.database_path = PathBuf::from(v.trim());
        }
        if let Ok(v) = std::env::var("CLIPFORGE_MAX_UPLOAD_BYTES")
            && !v.trim().is_empty()
        {
            cfg.max_upload_bytes = v
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("invalid CLIPFORGE_MAX_UPLOAD_BYTES: {e}"))?;
        }
        if let Ok(v) = std::env::var("CLIPFORGE_CHUNK_SIZE_BYTES")
            && !v.trim().is_empty()
        {
            let parsed = v
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("invalid CLIPFORGE_CHUNK_SIZE_BYTES: {e}"))?;
            if parsed == 0 {
                return Err("CLIPFORGE_CHUNK_SIZE_BYTES must be positive".to_string());
            }
            cfg.chunk_size_bytes = parsed;
        }
        if let Ok(v) = std::env::var("CLIPFORGE_DEFAULT_QUOTA_BYTES")
            && !v.trim().is_empty()
        {
            cfg.default_quota_bytes = v
                .trim()
                .parse::<u64>()
                .map_err(|e| format!("invalid CLIPFORGE_DEFAULT_QUOTA_BYTES: {e}"))?;
        }
        if let Ok(v) = std::env::var("CLIPFORGE_PUBLIC_BASE_URL")
            && !v.trim().is_empty()
        {
            cfg.public_base_url = Some(v.trim().trim_end_matches('/').to_string());
        }
        if let Ok(v) = std::env::var("CLIPFORGE_WEB_DIR")
            && !v.trim().is_empty()
        {
            cfg.web_dir = Some(PathBuf::from(v.trim()));
        }
        if let Ok(v) = std::env::var("CLIPFORGE_REGISTRATION_ENABLED")
            && !v.trim().is_empty()
        {
            cfg.registration_enabled = matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }
        if let Ok(v) = std::env::var("CLIPFORGE_SESSION_TTL_HOURS")
            && !v.trim().is_empty()
        {
            cfg.session_ttl_hours = v
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("invalid CLIPFORGE_SESSION_TTL_HOURS: {e}"))?;
        }
        if let Ok(v) = std::env::var("CLIPFORGE_ADMIN_USERNAME")
            && !v.trim().is_empty()
        {
            cfg.admin_bootstrap_username = Some(v.trim().to_string());
        }
        if let Ok(v) = std::env::var("CLIPFORGE_ADMIN_PASSWORD")
            && !v.is_empty()
        {
            cfg.admin_bootstrap_password = Some(v);
        }

        let backend = std::env::var("CLIPFORGE_STORAGE_BACKEND")
            .unwrap_or_else(|_| "local".to_string())
            .trim()
            .to_ascii_lowercase();

        cfg.storage = match backend.as_str() {
            "local" => {
                let root_dir = std::env::var("CLIPFORGE_STORAGE_PATH")
                    .or_else(|_| std::env::var("CLIPFORGE_STORAGE_DIR"))
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("data/clips"));
                StorageConfig::Local { root_dir }
            }
            "s3" => {
                let s3 = S3Config::from_env().map_err(|e| e.to_string())?;
                StorageConfig::S3 {
                    endpoint: s3.endpoint,
                    bucket: s3.bucket,
                    region: s3.region,
                    access_key: s3.access_key,
                    secret_key: s3.secret_key,
                    force_path_style: s3.force_path_style,
                }
            }
            other => {
                return Err(format!("unsupported CLIPFORGE_STORAGE_BACKEND: {other}"));
            }
        };

        Ok(cfg)
    }
}
