//! Non-destructive storage diagnostics and opt-in write/read/delete probe.

use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::fs;
use uuid::Uuid;

use crate::key::validate_storage_key;
use crate::local::LocalStorage;
use crate::s3::{S3Config, S3Storage};
use crate::{StorageBackend, StorageConfig, StorageError};

/// Options controlling [`run_storage_check`].
#[derive(Debug, Clone)]
pub struct StorageCheckOptions {
    /// Must be explicitly `true` to perform a write/read/delete probe.
    /// Defaults to `false` (strictly read-only).
    pub allow_write_probe: bool,
    /// Dedicated temporary prefix used when `allow_write_probe` is `true`.
    pub probe_prefix: String,
}

impl Default for StorageCheckOptions {
    fn default() -> Self {
        Self {
            allow_write_probe: false,
            probe_prefix: "_clipforge_probe".to_string(),
        }
    }
}

/// Outcome status of a single diagnostic check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skipped,
}

/// Individual diagnostic check item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCheckItem {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

/// Verification status of a storage capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Verified,
    SupportedUnverified,
    Unavailable(String),
}

/// Summary of backend capabilities observed during [`run_storage_check`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCapabilities {
    pub read: CapabilityStatus,
    pub write: CapabilityStatus,
    pub range_read: CapabilityStatus,
    pub multipart: CapabilityStatus,
    pub delete: CapabilityStatus,
}

/// Full diagnostic report returned by [`run_storage_check`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageCheckReport {
    pub backend: String,
    pub target: String,
    pub healthy: bool,
    pub checks: Vec<StorageCheckItem>,
    pub capabilities: StorageCapabilities,
}

/// Runs non-destructive storage diagnostics (and an optional write/read/delete probe
/// only when `options.allow_write_probe` is `true`).
pub async fn run_storage_check(
    config: &StorageConfig,
    options: &StorageCheckOptions,
) -> StorageCheckReport {
    match config {
        StorageConfig::Local { root_dir } => check_local(root_dir, options).await,
        StorageConfig::S3 {
            endpoint,
            bucket,
            region,
            access_key,
            secret_key,
            force_path_style,
        } => {
            let s3_cfg = S3Config {
                endpoint: endpoint.clone(),
                bucket: bucket.clone(),
                region: region.clone(),
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                force_path_style: *force_path_style,
                connect_timeout: std::time::Duration::from_secs(10),
                read_timeout: std::time::Duration::from_secs(30),
            };
            check_s3(&s3_cfg, options).await
        }
    }
}

async fn check_local(
    root_dir: &std::path::Path,
    options: &StorageCheckOptions,
) -> StorageCheckReport {
    let mut checks = Vec::new();
    let mut healthy = true;

    if root_dir.as_os_str().is_empty() {
        healthy = false;
        checks.push(StorageCheckItem {
            name: "config_validation".to_string(),
            status: CheckStatus::Fail,
            detail: "root_dir is empty".to_string(),
        });
        return StorageCheckReport {
            backend: "local".to_string(),
            target: root_dir.display().to_string(),
            healthy,
            checks,
            capabilities: unavailable_caps("invalid local config"),
        };
    }

    checks.push(StorageCheckItem {
        name: "config_validation".to_string(),
        status: CheckStatus::Pass,
        detail: format!("root_dir={}", root_dir.display()),
    });

    let dir_ok = match fs::metadata(root_dir).await {
        Ok(meta) if meta.is_dir() => {
            checks.push(StorageCheckItem {
                name: "directory_access".to_string(),
                status: CheckStatus::Pass,
                detail: "root directory exists and is accessible".to_string(),
            });
            true
        }
        Ok(_) => {
            healthy = false;
            checks.push(StorageCheckItem {
                name: "directory_access".to_string(),
                status: CheckStatus::Fail,
                detail: "root_dir exists but is not a directory".to_string(),
            });
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            checks.push(StorageCheckItem {
                name: "directory_access".to_string(),
                status: CheckStatus::Warn,
                detail: "root directory does not exist yet (will be created on first write)"
                    .to_string(),
            });
            true
        }
        Err(e) => {
            healthy = false;
            checks.push(StorageCheckItem {
                name: "directory_access".to_string(),
                status: CheckStatus::Fail,
                detail: format!("cannot stat root_dir: {e}"),
            });
            false
        }
    };

    let mut caps = if dir_ok {
        supported_unverified_caps()
    } else {
        unavailable_caps("directory inaccessible")
    };

    if dir_ok && options.allow_write_probe {
        let storage = LocalStorage::new(root_dir);
        let (probe_item, probed_ok) = run_write_probe(&storage, &options.probe_prefix).await;
        if !probed_ok {
            healthy = false;
            caps = unavailable_caps(&probe_item.detail);
        } else {
            caps = verified_caps();
        }
        checks.push(probe_item);
    } else {
        checks.push(StorageCheckItem {
            name: "write_probe".to_string(),
            status: CheckStatus::Skipped,
            detail: "read-only mode (allow_write_probe=false)".to_string(),
        });
    }

    StorageCheckReport {
        backend: "local".to_string(),
        target: root_dir.display().to_string(),
        healthy,
        checks,
        capabilities: caps,
    }
}

async fn check_s3(config: &S3Config, options: &StorageCheckOptions) -> StorageCheckReport {
    let mut checks = Vec::new();
    let mut healthy = true;
    let target = format!(
        "{}/{}",
        config.endpoint.trim_end_matches('/'),
        config.bucket
    );

    let url = match config.validate() {
        Ok(u) => {
            if u.scheme() == "http" && !is_local_host(u.host_str().unwrap_or_default()) {
                checks.push(StorageCheckItem {
                    name: "config_validation".to_string(),
                    status: CheckStatus::Warn,
                    detail: "using plaintext HTTP for non-localhost S3 endpoint".to_string(),
                });
            } else {
                checks.push(StorageCheckItem {
                    name: "config_validation".to_string(),
                    status: CheckStatus::Pass,
                    detail: format!(
                        "endpoint={}, bucket={}, region={}, force_path_style={}",
                        config.endpoint, config.bucket, config.region, config.force_path_style
                    ),
                });
            }
            u
        }
        Err(e) => {
            checks.push(StorageCheckItem {
                name: "config_validation".to_string(),
                status: CheckStatus::Fail,
                detail: e.to_string(),
            });
            return StorageCheckReport {
                backend: "s3".to_string(),
                target,
                healthy: false,
                checks,
                capabilities: unavailable_caps("invalid S3 config"),
            };
        }
    };

    let host = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().unwrap_or(443);

    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(mut addrs) => {
            if addrs.next().is_some() {
                checks.push(StorageCheckItem {
                    name: "dns_resolution".to_string(),
                    status: CheckStatus::Pass,
                    detail: format!("resolved {host}:{port}"),
                });
            } else {
                healthy = false;
                checks.push(StorageCheckItem {
                    name: "dns_resolution".to_string(),
                    status: CheckStatus::Fail,
                    detail: format!("no addresses returned for {host}:{port}"),
                });
            }
        }
        Err(e) => {
            healthy = false;
            checks.push(StorageCheckItem {
                name: "dns_resolution".to_string(),
                status: CheckStatus::Fail,
                detail: format!("DNS lookup failed for {host}:{port}: {e}"),
            });
        }
    }

    let s3 = match S3Storage::new(config.clone()) {
        Ok(s) => s,
        Err(e) => {
            checks.push(StorageCheckItem {
                name: "client_init".to_string(),
                status: CheckStatus::Fail,
                detail: e.to_string(),
            });
            return StorageCheckReport {
                backend: "s3".to_string(),
                target,
                healthy: false,
                checks,
                capabilities: unavailable_caps("S3 client initialization failed"),
            };
        }
    };

    let has_creds = !config.access_key.trim().is_empty() && !config.secret_key.trim().is_empty();
    let mut caps = supported_unverified_caps();

    if has_creds {
        match s3.check_bucket().await {
            Ok(()) => {
                checks.push(StorageCheckItem {
                    name: "tls_and_bucket_auth".to_string(),
                    status: CheckStatus::Pass,
                    detail: format!(
                        "TLS/connectivity and bucket '{}' access verified",
                        config.bucket
                    ),
                });
            }
            Err(StorageError::Unavailable(msg)) => {
                healthy = false;
                checks.push(StorageCheckItem {
                    name: "tls_and_bucket_auth".to_string(),
                    status: CheckStatus::Fail,
                    detail: msg.clone(),
                });
                caps = unavailable_caps(&msg);
            }
            Err(e) => {
                healthy = false;
                let msg = e.to_string();
                checks.push(StorageCheckItem {
                    name: "tls_and_bucket_auth".to_string(),
                    status: CheckStatus::Fail,
                    detail: msg.clone(),
                });
                caps = unavailable_caps(&msg);
            }
        }
    } else {
        // Perform an unauthenticated TLS/TCP reachability check if HTTPS/HTTP.
        let reach_client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .danger_accept_invalid_certs(false)
            .build();
        if let Ok(client) = reach_client {
            match client.head(url.clone()).send().await {
                Ok(resp) => {
                    checks.push(StorageCheckItem {
                        name: "tls_connectivity".to_string(),
                        status: CheckStatus::Pass,
                        detail: format!("endpoint reachable (HTTP {})", resp.status()),
                    });
                }
                Err(e) => {
                    healthy = false;
                    checks.push(StorageCheckItem {
                        name: "tls_connectivity".to_string(),
                        status: CheckStatus::Fail,
                        detail: format!("endpoint unreachable: {e}"),
                    });
                    caps = unavailable_caps(&e.to_string());
                }
            }
        }
        checks.push(StorageCheckItem {
            name: "bucket_auth".to_string(),
            status: CheckStatus::Skipped,
            detail: "no S3 credentials provided; auth and bucket permissions unverified"
                .to_string(),
        });
    }

    if healthy && has_creds && options.allow_write_probe {
        let (probe_item, probed_ok) = run_write_probe(&s3, &options.probe_prefix).await;
        if !probed_ok {
            healthy = false;
            caps = unavailable_caps(&probe_item.detail);
        } else {
            caps = verified_caps();
        }
        checks.push(probe_item);
    } else {
        checks.push(StorageCheckItem {
            name: "write_probe".to_string(),
            status: CheckStatus::Skipped,
            detail: if !options.allow_write_probe {
                "read-only mode (allow_write_probe=false)".to_string()
            } else {
                "skipped because credentials or connectivity check did not pass".to_string()
            },
        });
    }

    StorageCheckReport {
        backend: "s3".to_string(),
        target,
        healthy,
        checks,
        capabilities: caps,
    }
}

async fn run_write_probe(backend: &dyn StorageBackend, prefix: &str) -> (StorageCheckItem, bool) {
    let clean_prefix = prefix.trim_matches('/');
    let probe_key = format!("{clean_prefix}/{}.bin", Uuid::new_v4());
    if let Err(e) = validate_storage_key(&probe_key) {
        return (
            StorageCheckItem {
                name: "write_probe".to_string(),
                status: CheckStatus::Fail,
                detail: format!("invalid probe prefix: {e}"),
            },
            false,
        );
    }

    let payload = Bytes::from_static(b"clipforge-storage-probe-v1");
    if let Err(e) = backend
        .put_bytes(&probe_key, payload.clone(), "application/octet-stream")
        .await
    {
        return (
            StorageCheckItem {
                name: "write_probe".to_string(),
                status: CheckStatus::Fail,
                detail: format!("put_bytes failed: {e}"),
            },
            false,
        );
    }

    let head_ok = backend
        .head_object(&probe_key)
        .await
        .map(|m| m.size_bytes == payload.len() as u64);
    if !matches!(head_ok, Ok(true)) {
        let _ = backend.delete_object(&probe_key).await;
        return (
            StorageCheckItem {
                name: "write_probe".to_string(),
                status: CheckStatus::Fail,
                detail: "head_object verification failed".to_string(),
            },
            false,
        );
    }

    let range_res = backend.get_object_range(&probe_key, 0, Some(9)).await;
    let mut read_back = Vec::new();
    match range_res {
        Ok(mut obj) => {
            while let Some(chunk) = obj.stream.next().await {
                match chunk {
                    Ok(c) => read_back.extend_from_slice(&c),
                    Err(e) => {
                        let _ = backend.delete_object(&probe_key).await;
                        return (
                            StorageCheckItem {
                                name: "write_probe".to_string(),
                                status: CheckStatus::Fail,
                                detail: format!("range stream error: {e}"),
                            },
                            false,
                        );
                    }
                }
            }
        }
        Err(e) => {
            let _ = backend.delete_object(&probe_key).await;
            return (
                StorageCheckItem {
                    name: "write_probe".to_string(),
                    status: CheckStatus::Fail,
                    detail: format!("get_object_range failed: {e}"),
                },
                false,
            );
        }
    }

    if read_back != payload[..10] {
        let _ = backend.delete_object(&probe_key).await;
        return (
            StorageCheckItem {
                name: "write_probe".to_string(),
                status: CheckStatus::Fail,
                detail: "range read-back content mismatch".to_string(),
            },
            false,
        );
    }

    if let Err(e) = backend.delete_object(&probe_key).await {
        return (
            StorageCheckItem {
                name: "write_probe".to_string(),
                status: CheckStatus::Fail,
                detail: format!("delete_object cleanup failed: {e}"),
            },
            false,
        );
    }

    (
        StorageCheckItem {
            name: "write_probe".to_string(),
            status: CheckStatus::Pass,
            detail: format!("put/head/range-get/delete probe succeeded under {clean_prefix}/"),
        },
        true,
    )
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn supported_unverified_caps() -> StorageCapabilities {
    StorageCapabilities {
        read: CapabilityStatus::SupportedUnverified,
        write: CapabilityStatus::SupportedUnverified,
        range_read: CapabilityStatus::SupportedUnverified,
        multipart: CapabilityStatus::SupportedUnverified,
        delete: CapabilityStatus::SupportedUnverified,
    }
}

fn verified_caps() -> StorageCapabilities {
    StorageCapabilities {
        read: CapabilityStatus::Verified,
        write: CapabilityStatus::Verified,
        range_read: CapabilityStatus::Verified,
        multipart: CapabilityStatus::SupportedUnverified,
        delete: CapabilityStatus::Verified,
    }
}

fn unavailable_caps(reason: &str) -> StorageCapabilities {
    let msg = reason.to_string();
    StorageCapabilities {
        read: CapabilityStatus::Unavailable(msg.clone()),
        write: CapabilityStatus::Unavailable(msg.clone()),
        range_read: CapabilityStatus::Unavailable(msg.clone()),
        multipart: CapabilityStatus::Unavailable(msg.clone()),
        delete: CapabilityStatus::Unavailable(msg),
    }
}
