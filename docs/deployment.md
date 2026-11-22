# Deployment & Reverse Proxy Guide

This guide covers deploying `clipforge-server` with Docker Compose behind an HTTPS reverse proxy.

## 1. Choose a Storage Mode

Copy `.env.example` to `.env`:

```bash
cp .env.example .env
```

### Option A: External RustFS / S3 (Default)

`compose.yaml` connects to an external S3-compatible endpoint (no local S3/MinIO container is started):

```dotenv
CLIPFORGE_STORAGE_BACKEND=s3
S3_ENDPOINT=https://s3.example.com
S3_BUCKET=clipforge
S3_REGION=us-east-1
S3_FORCE_PATH_STYLE=true
S3_ACCESS_KEY_ID=your-access-key
S3_SECRET_ACCESS_KEY=your-secret-key
```

### Option B: Local Filesystem Storage (Zero S3 Credentials)

Store clips directly in the persistent `/data` Docker volume:

```dotenv
CLIPFORGE_STORAGE_BACKEND=local
CLIPFORGE_STORAGE_PATH=/data/clips
```

Leave `S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY` empty.

## 2. Core Server Settings

Set your public URL and bind address in `.env`:

```dotenv
# Bind only to localhost when a reverse proxy runs on the same host
CLIPFORGE_HOST_BIND=127.0.0.1:3000

# External HTTPS URL used to construct share links (no trailing slash)
CLIPFORGE_PUBLIC_BASE_URL=https://clips.example.com

# Optional: match your reverse proxy upload limit (default 500 MiB)
CLIPFORGE_MAX_UPLOAD_BYTES=524288000
```

Start the service:

```bash
docker compose up -d --build
```

Verify the container healthcheck:

```bash
docker compose ps
curl -fsS http://127.0.0.1:3000/health
```

## 3. Reverse Proxy Configuration

`clipforge-server` expects TLS termination at a reverse proxy. Because clip uploads can be large (up to 500 MiB by default) and video playback uses HTTP `Range` requests, configure your proxy with:

- Sufficient request body size limit (`512m` / `512MB` or higher if `CLIPFORGE_MAX_UPLOAD_BYTES` is increased)
- Disabled request/response buffering for streaming uploads and Range playback
- Extended read/send timeouts (300s+)
- Standard `X-Forwarded-For`, `X-Forwarded-Proto`, and `Host` headers

---

### Nginx Proxy Manager (NPM)

1. In NPM, go to **Hosts -> Proxy Hosts -> Add Proxy Host**:
   - **Domain Names**: `clips.example.com`
   - **Scheme**: `http`
   - **Forward Hostname / IP**: `127.0.0.1` (or your Docker host IP if NPM runs in a separate container network)
   - **Forward Port**: `3000`
   - **Block Common Exploits**: Enabled
2. Under **SSL**:
   - Select your certificate (or request a Let's Encrypt certificate)
   - Enable **Force SSL** and **HTTP/2 Support**
3. Under **Advanced -> Custom Nginx Configuration**, add:

```nginx
client_max_body_size 512m;
proxy_request_buffering off;
proxy_buffering off;
proxy_read_timeout 300s;
proxy_send_timeout 300s;
```

---

### Nginx

```nginx
server {
    listen 80;
    listen [::]:80;
    server_name clips.example.com;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;
    server_name clips.example.com;

    ssl_certificate     /etc/letsencrypt/live/clips.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/clips.example.com/privkey.pem;

    # Match or exceed CLIPFORGE_MAX_UPLOAD_BYTES
    client_max_body_size 512m;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;

        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-Host $host;

        # Stream uploads and HTTP Range video responses without disk buffering
        proxy_request_buffering off;
        proxy_buffering off;

        proxy_connect_timeout 30s;
        proxy_send_timeout 300s;
        proxy_read_timeout 300s;
    }
}
```

---

### Caddy

```caddyfile
clips.example.com {
    request_body {
        max_size 512MB
    }

    reverse_proxy 127.0.0.1:3000 {
        flush_interval -1
        transport http {
            read_timeout 300s
            write_timeout 300s
        }
    }
}
```

Caddy automatically provisions TLS certificates and sets `X-Forwarded-For`, `X-Forwarded-Proto`, and `X-Forwarded-Host`.

## 4. Operations & Upgrades

- **Check storage connectivity**:
  ```bash
  docker compose exec clipforge-server clipforge-server storage check
  ```
- **Resource limits**: `compose.yaml` sets a baseline limit of `2.0` CPUs and `1G` RAM (`deploy.resources.limits`). Adjust if hosting high-concurrency video streaming.
- **Upgrading**:
  ```bash
  git pull
  docker compose up -d --build
  ```
  SQLite migrations run automatically on startup. Back up your database before upgrading (see [docs/backup-restore.md](backup-restore.md)).
