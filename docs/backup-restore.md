# Backup & Restore Guide

ClipForge Server stores metadata (users, API tokens, clip records, upload sessions, and share links) in SQLite at `/data/clipforge-server.db`.

> **Important:** Never copy a live SQLite database file directly with `cp` while the server is running—in WAL mode, uncommitted transactions live in `-wal` and `-shm` files and copying `.db` alone can produce a corrupt backup. Always use SQLite's online backup API (`VACUUM INTO` or `.backup`) or stop the server first.

## 1. Backing Up SQLite

### Online Backup (Zero Downtime)

If `sqlite3` is available on the host (or via an ephemeral container mounting the volume):

```bash
mkdir -p ./backups
BACKUP_FILE="clipforge-server-$(date -u +%Y%m%dT%H%M%SZ).db"

# Using a temporary alpine/sqlite container against the named volume
docker run --rm \
  -v clipforge_data:/data:ro \
  -v "$(pwd)/backups:/backups" \
  keinos/sqlite3 \
  sqlite3 /data/clipforge-server.db ".backup '/backups/${BACKUP_FILE}'"
```

### Offline Backup (Brief Stop)

If you prefer a simple file copy:

```bash
docker compose stop clipforge-server

docker run --rm \
  -v clipforge_data:/data:ro \
  -v "$(pwd)/backups:/backups" \
  alpine:3.20 \
  sh -c "cp /data/clipforge-server.db* /backups/"

docker compose start clipforge-server
```

## 2. Backing Up Clip Media

- **External RustFS / S3 mode (`CLIPFORGE_STORAGE_BACKEND=s3`)**:
  Media objects live in your S3/RustFS bucket (`S3_BUCKET`, e.g. on `https://s3.example.com`) under `clips/{user_id}/{clip_id}/...`. Use your object store's bucket replication, snapshots, or `rclone` / `aws s3 sync` to back up the bucket.
- **Local filesystem mode (`CLIPFORGE_STORAGE_BACKEND=local`)**:
  Media objects live in `/data/clips` inside the `clipforge_data` volume. Back up the `/data/clips` directory alongside the SQLite backup:

  ```bash
  docker run --rm \
    -v clipforge_data:/data:ro \
    -v "$(pwd)/backups:/backups" \
    alpine:3.20 \
    tar -czf "/backups/clips-$(date -u +%Y%m%dT%H%M%SZ).tar.gz" -C /data clips
  ```

## 3. Restoring from Backup

1. **Stop the server** so no process holds the SQLite database open:
   ```bash
   docker compose stop clipforge-server
   ```

2. **Restore the SQLite database** (and remove any stale `-wal`/`-shm` files, then ensure UID `10001` owns `/data`):
   ```bash
   docker run --rm \
     -v clipforge_data:/data \
     -v "$(pwd)/backups:/backups:ro" \
     alpine:3.20 \
     sh -c '
       rm -f /data/clipforge-server.db-wal /data/clipforge-server.db-shm &&
       cp "/backups/clipforge-server-YYYYMMDDTHHMMSSZ.db" /data/clipforge-server.db &&
       chown -R 10001:10001 /data
     '
   ```

3. *(Local storage mode only)* **Restore `/data/clips`**:
   ```bash
   docker run --rm \
     -v clipforge_data:/data \
     -v "$(pwd)/backups:/backups:ro" \
     alpine:3.20 \
     sh -c '
       tar -xzf "/backups/clips-YYYYMMDDTHHMMSSZ.tar.gz" -C /data &&
       chown -R 10001:10001 /data/clips
     '
   ```

4. **Start the server and verify**:
   ```bash
   docker compose start clipforge-server
   curl -fsS http://127.0.0.1:3000/health
   docker compose exec clipforge-server clipforge-server storage check
   ```
