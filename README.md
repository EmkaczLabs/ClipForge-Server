# ClipForge Server

Standalone, self-hostable media upload, storage, and share-link server for [ClipForge](https://github.com/EmkaczLabs/ClipForge) ([ClipForge-Server](https://github.com/EmkaczLabs/ClipForge-Server)), written in Rust with Axum, SQLite, and pluggable object storage (local disk or external RustFS / S3-compatible).

## Features

- **Streaming & Resumable Uploads**: Multipart upload sessions with per-user storage quota enforcement.
- **Pluggable Storage Backends**:
  - **Local Filesystem** (default): Zero-credential local disk mode (`CLIPFORGE_STORAGE_BACKEND=local`) for single-node deployments and CI/testing.
  - **External RustFS / S3**: Connects to an external S3-compatible object store (such as `https://s3.example.com`) using AWS SigV4 with no bundled S3 container required.
- **Unlisted Share Pages**: Dark-first HTML5 video player at `/c/{share_id}` with HTTP `Range` seeking support.
- **Token-Based Authentication**: Argon2id password hashing and SHA-256 hashed personal API tokens; S3 credentials never leave the server.
- **Single-Binary & Container Ready**: Embedded SQLite (`rusqlite` bundled), health endpoints (`/health`, `/api/v1/health`), and non-root Docker image.

## Quick Start (Docker Compose)

1. Copy the environment template:

   ```bash
   cp .env.example .env
   ```

2. Choose your storage mode in `.env`:
   - **Local filesystem (default, zero S3 credentials)**: Uses `CLIPFORGE_STORAGE_BACKEND=local` and `CLIPFORGE_STORAGE_PATH=/data/clips`.
   - **External RustFS / S3**: Set `CLIPFORGE_STORAGE_BACKEND=s3`, `S3_ENDPOINT` (e.g., `https://s3.example.com`), `S3_BUCKET`, `S3_ACCESS_KEY_ID`, and `S3_SECRET_ACCESS_KEY`.

3. Set `CLIPFORGE_PUBLIC_BASE_URL` to your external HTTPS URL (for example `https://clips.example.com`).

4. Start the server:

   ```bash
   docker compose up -d --build
   ```

5. Verify health:

   ```bash
   curl -fsS http://127.0.0.1:3000/health
   ```

For reverse proxy setup (Nginx Proxy Manager, Nginx, Caddy), see [docs/deployment.md](docs/deployment.md).

## Architecture

| Crate | Responsibility |
| --- | --- |
| `clipforge-api` | Shared `/api/v1` DTOs and OpenAPI 3.1 specification |
| `clipforge-db` | SQLite schema, migrations, and repository queries |
| `clipforge-auth` | Password hashing, personal API tokens, and authorization checks |
| `clipforge-storage` | `StorageBackend` trait with Local FS and S3/RustFS implementations |
| `clipforge-server` | Axum HTTP server, CLI (`serve`, `storage check`, `admin`), and static web player |

## Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `CLIPFORGE_BIND_ADDR` | `0.0.0.0:3000` | HTTP listen address inside the container/process |
| `CLIPFORGE_HOST_BIND` | `127.0.0.1:3000` | Host bind address used by `compose.yaml` |
| `CLIPFORGE_DATABASE_PATH` | `clipforge-server.db` (`/data/clipforge-server.db` in Docker) | SQLite database file path |
| `CLIPFORGE_STORAGE_BACKEND` | `local` | Storage backend (`local` or `s3`) |
| `CLIPFORGE_STORAGE_PATH` | `data/clips` (`/data/clips` in Docker) | Root directory when `CLIPFORGE_STORAGE_BACKEND=local` |
| `S3_ENDPOINT` | — | S3/RustFS endpoint URL (e.g., `https://s3.example.com`) |
| `S3_BUCKET` | `clipforge` (Compose) | Target S3 bucket name |
| `S3_REGION` | `us-east-1` | S3 signing region |
| `S3_FORCE_PATH_STYLE` | `true` | Use path-style S3 requests (required for most RustFS/MinIO setups) |
| `S3_ACCESS_KEY_ID` | — | S3 access key (server-side only) |
| `S3_SECRET_ACCESS_KEY` | — | S3 secret key (server-side only) |
| `CLIPFORGE_PUBLIC_BASE_URL` | — | External base URL for share links |
| `CLIPFORGE_MAX_UPLOAD_BYTES` | `524288000` | Maximum clip upload size in bytes (500 MiB) |
| `CLIPFORGE_CHUNK_SIZE_BYTES` | `8388608` | Multipart chunk size in bytes (8 MiB) |
| `CLIPFORGE_REGISTRATION_ENABLED` | `true` | Allow self-registration via `/api/v1/auth/register` |
| `CLIPFORGE_SESSION_TTL_HOURS` | `720` | Default token TTL in hours (30 days) |
| `RUST_LOG` | `clipforge_server=info,tower_http=info` | Tracing log filter |

## CLI Commands

```bash
# Start the HTTP server (default)
clipforge-server serve --bind 0.0.0.0:3000

# Verify storage backend connectivity and configuration
clipforge-server storage check

# Administrative user management
clipforge-server admin list-users
clipforge-server admin create-user --username alice --password '...'
```

## Upgrade

To upgrade a Docker Compose deployment using the prebuilt image:

```bash
docker compose pull
docker compose up -d
```

Or if building from a local checkout:

```bash
git pull
docker compose up -d --build
```

SQLite migrations run automatically on startup. Back up your database before upgrading (see [docs/backup-restore.md](docs/backup-restore.md)).

## Documentation

- [Deployment & Reverse Proxy Guide](docs/deployment.md)
- [Backup & Restore Guide](docs/backup-restore.md)
- [Contributing](CONTRIBUTING.md)
- [Security Policy](SECURITY.md)
- [Changelog](CHANGELOG.md)

## License

Licensed under the [MIT License](LICENSE).
