# Security Policy

## Reporting a vulnerability

Please report suspected vulnerabilities privately to **the@lance.name** with
"SECURITY" in the subject. Do not open a public issue for security reports.

## Supported versions

Pre-1.0: only `main` and the latest release receive fixes.

## Notes for researchers

- The plugin runs as a FormLogic Desktop child process over stdio; it inherits
  no secrets (allow-listed env only).
- The event outbox (SQLite) protects transcript/SMS payloads at rest with
  per-user Windows DPAPI; `synchronous=FULL` durability.
- The voice HTTP service binds 127.0.0.1 only, rejects browser origins,
  bounds request bodies and concurrency.
- Driver installation uses an elevated helper whose SHA-256 is pinned into
  the plugin at build time; a byte of drift refuses to elevate. The
  self-signed driver-trust path is a development affordance — production
  driver signing is tracked in the FormLogic repo's `LAUNCH_CHECKLIST.md`.
