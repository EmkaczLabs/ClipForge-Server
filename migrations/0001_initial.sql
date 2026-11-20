-- ClipForge Server initial SQLite schema
-- Requires PRAGMA foreign_keys = ON

CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
    display_name TEXT,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('user', 'admin')),
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    quota_limit_bytes INTEGER NOT NULL DEFAULT 10737418240 CHECK (quota_limit_bytes >= 0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_users_username ON users(username);

CREATE TABLE IF NOT EXISTS storage_usage (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    used_bytes INTEGER NOT NULL DEFAULT 0 CHECK (used_bytes >= 0),
    reserved_bytes INTEGER NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    clip_count INTEGER NOT NULL DEFAULT 0 CHECK (clip_count >= 0),
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS api_tokens (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    scopes_json TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL,
    expires_at TEXT,
    last_used_at TEXT,
    revoked_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_api_tokens_user_id ON api_tokens(user_id);
CREATE INDEX IF NOT EXISTS idx_api_tokens_token_hash ON api_tokens(token_hash);

CREATE TABLE IF NOT EXISTS clips (
    id TEXT PRIMARY KEY NOT NULL,
    owner_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    game_json TEXT,
    duration_ms INTEGER NOT NULL DEFAULT 0 CHECK (duration_ms >= 0),
    width INTEGER NOT NULL DEFAULT 0 CHECK (width >= 0),
    height INTEGER NOT NULL DEFAULT 0 CHECK (height >= 0),
    fps REAL NOT NULL DEFAULT 0.0 CHECK (fps >= 0.0),
    size_bytes INTEGER NOT NULL DEFAULT 0 CHECK (size_bytes >= 0),
    codec TEXT NOT NULL DEFAULT '',
    content_type TEXT NOT NULL DEFAULT 'video/mp4',
    audio_tracks_json TEXT NOT NULL DEFAULT '[]',
    tags_json TEXT NOT NULL DEFAULT '[]',
    notes TEXT,
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('public', 'private')),
    created_at TEXT NOT NULL,
    uploaded_at TEXT NOT NULL,
    expires_at TEXT,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_clips_owner_uploaded ON clips(owner_id, uploaded_at DESC);
CREATE INDEX IF NOT EXISTS idx_clips_visibility ON clips(visibility);
CREATE INDEX IF NOT EXISTS idx_clips_expires_at ON clips(expires_at);

CREATE TABLE IF NOT EXISTS clip_assets (
    id TEXT PRIMARY KEY NOT NULL,
    clip_id TEXT NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
    owner_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    asset_kind TEXT NOT NULL CHECK (asset_kind IN ('original', 'thumbnail', 'preview')),
    storage_key TEXT NOT NULL UNIQUE,
    content_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL DEFAULT 0 CHECK (size_bytes >= 0),
    sha256 TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(clip_id, asset_kind)
);

CREATE INDEX IF NOT EXISTS idx_clip_assets_clip_id ON clip_assets(clip_id);
CREATE INDEX IF NOT EXISTS idx_clip_assets_owner_id ON clip_assets(owner_id);

CREATE TABLE IF NOT EXISTS uploads (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    clip_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'uploading', 'finalizing', 'completed', 'failed', 'aborted')),
    title TEXT NOT NULL,
    filename TEXT NOT NULL,
    content_type TEXT NOT NULL DEFAULT 'video/mp4',
    sha256 TEXT,
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('public', 'private')),
    metadata_json TEXT NOT NULL,
    storage_key TEXT NOT NULL,
    multipart_upload_id TEXT,
    total_bytes INTEGER NOT NULL CHECK (total_bytes >= 0),
    bytes_received INTEGER NOT NULL DEFAULT 0 CHECK (bytes_received >= 0),
    reserved_bytes INTEGER NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    part_size_bytes INTEGER NOT NULL CHECK (part_size_bytes > 0),
    total_parts INTEGER NOT NULL CHECK (total_parts >= 0),
    uploaded_parts_json TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_uploads_user_status ON uploads(user_id, status);
CREATE INDEX IF NOT EXISTS idx_uploads_expires_at ON uploads(expires_at);
CREATE INDEX IF NOT EXISTS idx_uploads_clip_id ON uploads(clip_id);

CREATE TABLE IF NOT EXISTS shares (
    share_id TEXT PRIMARY KEY NOT NULL,
    clip_id TEXT NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
    owner_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    visibility TEXT NOT NULL DEFAULT 'public' CHECK (visibility IN ('public', 'private')),
    created_at TEXT NOT NULL,
    expires_at TEXT,
    revoked_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_shares_clip_id ON shares(clip_id);
CREATE INDEX IF NOT EXISTS idx_shares_owner_id ON shares(owner_id);

CREATE TABLE IF NOT EXISTS server_settings (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
