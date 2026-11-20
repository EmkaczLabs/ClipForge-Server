# Contributing to ClipForge Server

Thank you for contributing to ClipForge Server!

## Prerequisites

- Rust toolchain (stable, Rust 1.85+ for Edition 2024) with `rustfmt` and `clippy`
- Docker & Docker Compose (optional, for container build and smoke testing)

## Local Development

Run all checks using local storage mode (no S3 credentials required):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

To run the server locally:

```bash
CLIPFORGE_STORAGE_BACKEND=local \
CLIPFORGE_STORAGE_PATH=./data/clips \
cargo run -p clipforge-server -- serve --bind 127.0.0.1:3000
```

## Guidelines

1. **Formatting & Lints**: All PRs must pass `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`.
2. **Tests**: Include unit or integration tests for new endpoints, database queries, auth rules, or storage behavior.
3. **Security**:
   - Never commit `.env` files, API tokens, or S3 credentials.
   - Use parameterized SQL queries (`rusqlite` params) and server-generated storage keys.
   - Preserve user isolation invariants (a user must never access or enumerate another user's private clips, uploads, or tokens).
4. **Pull Requests**: Keep changes focused and describe how you verified them.
