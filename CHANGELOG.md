# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Standalone `ClipForge-Server` workspace (`clipforge-api`, `clipforge-db`, `clipforge-auth`, `clipforge-storage`, `clipforge-server`).
- SQLite persistence with automatic startup migrations and per-user quota tracking.
- Argon2id password hashing and SHA-256 hashed personal API tokens.
- Pluggable storage backends: external RustFS / S3-compatible object storage and zero-credential local filesystem storage.
- Resumable multipart upload sessions and HTTP `Range` video streaming for `/c/{share_id}` share pages.
- Multi-stage non-root `Dockerfile`, `compose.yaml`, `.env.example`, GitHub Actions CI, and operational documentation (`docs/deployment.md`, `docs/backup-restore.md`).
