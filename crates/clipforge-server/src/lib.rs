//! ClipForge Server HTTP router, configuration, and application state.

pub mod config;
pub mod embed;
pub mod error;
pub mod media;
pub mod range;
pub mod routes;

pub use config::{DEFAULT_QUOTA_BYTES, ServerConfig};
pub use error::ServerError;
pub use routes::{AppState, build_router};
