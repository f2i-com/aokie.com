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

## Release bundle signing (TRUST-001)

Release bundles carry a first-party `package-manifest.json`: an Ed25519
signature (key id `fl-aokie-2026a`) over the SHA-256 + size of every bundle
file, produced by `crates/package-signer` in the release pipeline. FormLogic
Desktop pins the PUBLIC key and refuses to launch a plugin directory whose
signature or digests do not verify (tampered = quarantined). The signing seed
lives only in the CI secret store (`AOKIE_PACKAGE_SIGNING_KEY`) and the
operator's offline key file — never in this repository. Tag releases FAIL
without both the Authenticode certificate and the package signing key; verify
a downloaded bundle offline with:

```
cargo run -p package-signer -- verify --dir <bundle> --pubkey n832sELL2yC6UksKhiz4C4UY7P9//mf8JfeFJRcuk5s=
```

**The bundle directory is immutable.** Verification fails on any file whose
digest differs AND on any file the manifest does not list, so nothing may be
written into an installed bundle after signing — not runtime state, not a
`.bak` left beside a replaced binary. A host that hands the plugin a writable
directory must place it OUTSIDE the bundle; OAIY Desktop used to use
`<plugin>/data`, and the plugin's first settings write permanently invalidated
its own signature (fixed by moving it to `<dataDir>/plugin-data/<id>`). If
`verify` reports a digest mismatch on `SHA256SUMS.txt` or `release-manifest.json`
in particular, suspect a local edit that updated one of those without re-signing;
re-sign only after the directory is back to exactly the files that belong in it.
