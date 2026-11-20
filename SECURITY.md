# Security Policy

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report security issues privately to the maintainers via GitHub Security Advisories (the **Security** tab -> **Report a vulnerability**) or by contacting the repository maintainers directly. Include:

- Affected component and version/commit
- Steps to reproduce or proof-of-concept
- Potential impact

We will acknowledge receipt and work on a fix and coordinated disclosure.

## Security Model & Operator Responsibilities

- **Credential Isolation**: S3/RustFS credentials (`S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`) are server-side only and are never exposed to desktop clients or share pages.
- **Authentication**: User passwords are hashed with Argon2id; personal API tokens are cryptographically random and stored as SHA-256 digests.
- **Storage Key Safety**: Object keys are server-generated (`clips/{user_id}/{clip_id}/...`) and validated against path traversal.
- **Transport Security**: Always terminate TLS at a reverse proxy (Nginx, Nginx Proxy Manager, or Caddy) in production and bind the application port to `127.0.0.1` (`CLIPFORGE_HOST_BIND=127.0.0.1:3000`).
- **Unlisted Shares**: Share links (`/c/{share_id}`) are unlisted capabilities accessible to anyone holding the link unless revoked or expired.
