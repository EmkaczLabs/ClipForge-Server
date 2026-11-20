use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, Subcommand};
use clipforge_db::Database;
use clipforge_server::{AppState, ServerConfig, build_router};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "clipforge-server",
    version,
    about = "ClipForge standalone media server"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the HTTP API server (default).
    Serve {
        /// Address to bind to (overrides CLIPFORGE_BIND_ADDR).
        #[arg(long, env = "CLIPFORGE_BIND_ADDR")]
        bind: Option<String>,
    },
    /// Verify object storage connectivity and configuration.
    Storage {
        #[command(subcommand)]
        action: StorageCommand,
    },
    /// Administrative commands for users, tokens, and quotas.
    Admin {
        #[command(subcommand)]
        action: AdminCommand,
    },
}

#[derive(Debug, Subcommand)]
enum StorageCommand {
    /// Check storage backend health and bucket/directory access.
    Check,
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// List users in the database.
    ListUsers,
    /// Create a new user account.
    CreateUser {
        #[arg(long)]
        username: String,
        #[arg(long)]
        password: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("clipforge_server=info,tower_http=info")),
        )
        .init();

    let cli = Cli::parse();
    let mut config = ServerConfig::default();

    match cli.command.unwrap_or(Command::Serve { bind: None }) {
        Command::Serve { bind } => {
            if let Some(addr) = bind {
                config.bind_addr = addr;
            }
            let db = Database::open(&config.database_path).await?;
            let state = AppState {
                config: Arc::new(config.clone()),
                db,
                started_at: Instant::now(),
            };
            let app = build_router(state);
            let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
            tracing::info!(bind_addr = %config.bind_addr, "clipforge-server listening");
            axum::serve(listener, app).await?;
        }
        Command::Storage { action } => match action {
            StorageCommand::Check => {
                tracing::info!("storage check command (skeleton)");
            }
        },
        Command::Admin { action } => match action {
            AdminCommand::ListUsers => {
                tracing::info!("admin list-users command (skeleton)");
            }
            AdminCommand::CreateUser { username, .. } => {
                tracing::info!(%username, "admin create-user command (skeleton)");
            }
        },
    }

    Ok(())
}
