use std::sync::Arc;

use clap::{Parser, Subcommand};
use clipforge_api::UserRole;
use clipforge_auth::{bootstrap_admin, create_user};
use clipforge_db::Database;
use clipforge_server::{AppState, ServerConfig, build_router};
use clipforge_storage::{StorageCheckOptions, create_storage_backend, run_storage_check};
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
    Check {
        /// Perform an opt-in write/read/delete probe under a dedicated prefix.
        #[arg(long, default_value_t = false)]
        write_probe: bool,
        /// Emit report as JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// List users in the database.
    ListUsers,
    /// Create (or bootstrap) an admin user account.
    Create {
        #[arg(long)]
        username: String,
        #[arg(long)]
        password: String,
        #[arg(long)]
        quota_bytes: Option<u64>,
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
    let mut config = ServerConfig::from_env().map_err(|e| anyhow::anyhow!(e))?;

    match cli.command.unwrap_or(Command::Serve { bind: None }) {
        Command::Serve { bind } => {
            if let Some(addr) = bind {
                config.bind_addr = addr;
            }
            let db = Database::open(&config.database_path).await?;
            if let (Some(admin_user), Some(admin_pass)) = (
                config.admin_bootstrap_username.as_deref(),
                config.admin_bootstrap_password.as_deref(),
            ) {
                match bootstrap_admin(&db, admin_user, admin_pass, config.default_quota_bytes).await
                {
                    Ok(Some(created)) => {
                        tracing::info!(username = %created.username, "bootstrapped initial admin user");
                    }
                    Ok(None) => {
                        tracing::debug!("skipping admin bootstrap: users already exist");
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("admin bootstrap failed: {e}"));
                    }
                }
            }

            let storage = create_storage_backend(&config.storage)?;
            let state = AppState::new(Arc::new(config.clone()), db, storage);
            let app = build_router(state);
            let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
            tracing::info!(bind_addr = %config.bind_addr, "clipforge-server listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
        Command::Storage { action } => match action {
            StorageCommand::Check { write_probe, json } => {
                let options = StorageCheckOptions {
                    allow_write_probe: write_probe,
                    ..StorageCheckOptions::default()
                };
                let report = run_storage_check(&config.storage, &options).await;
                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!(
                        "Storage backend: {} (target: {}, healthy: {})",
                        report.backend, report.target, report.healthy
                    );
                    for item in &report.checks {
                        println!("  [{:?}] {}: {}", item.status, item.name, item.detail);
                    }
                }
                if !report.healthy {
                    std::process::exit(1);
                }
            }
        },
        Command::Admin { action } => {
            let db = Database::open(&config.database_path).await?;
            match action {
                AdminCommand::ListUsers => {
                    let users = db.list_users(500, 0).await?;
                    for user in users {
                        println!(
                            "{}\t{}\t{:?}\tactive={}",
                            user.id, user.username, user.role, user.is_active
                        );
                    }
                }
                AdminCommand::Create {
                    username,
                    password,
                    quota_bytes,
                } => {
                    let created = create_user(
                        &db,
                        &username,
                        &password,
                        Some("Administrator".to_string()),
                        UserRole::Admin,
                        quota_bytes.unwrap_or(config.default_quota_bytes),
                    )
                    .await?;
                    println!(
                        "Created admin user '{}' (id: {})",
                        created.username, created.id
                    );
                }
            }
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, draining connections");
}
