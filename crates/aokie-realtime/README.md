# Aokie realtime gateway

This crate is the vendor-neutral signalling/control service for Aokie
Companion. FormLogic can mint admissions and publish Desktop presence, but the
wire protocol and gateway are self-hostable and do not depend on FormLogic's
database or PHP runtime.

The current vertical slice provides authenticated WebSocket admission,
app/device isolation, one authoritative Desktop publisher per app, bounded
snapshot replay, idle synchronization, mobile command forwarding and typed
acknowledgement routing. It carries no PCM, SDP, ICE, bearer credential or
media key in durable storage. The media bridge remains disabled until the
Desktop and native WebRTC endpoints are implemented.

## Local development

Create an admission JSON array in `AOKIE_GATEWAY_ADMISSIONS` and run the
service. Tokens are SHA-256 indexed in memory and never logged.

```powershell
$env:AOKIE_GATEWAY_BIND='127.0.0.1:18787'
$env:AOKIE_GATEWAY_ADMISSIONS='[{"token":"replace-with-32-random-characters","role":"mobile","appId":"app_local","subjectId":"device_local","grants":[]},{"token":"replace-with-another-32-random-token","role":"desktop","appId":"app_local","subjectId":"desktop_local","grants":[]}]'
cargo run -p aokie-realtime
```

The native debug Companion may connect to
`ws://127.0.0.1:18787/v1/realtime`. Release clients require `wss://`. For a
remote or self-hosted deployment, terminate TLS at a maintained reverse proxy,
keep the gateway on a private listener, and replace static local admissions
with short-lived, device-bound tickets from the deployment's identity service.
The binary refuses a non-loopback plain-HTTP bind unless the operator
explicitly sets `AOKIE_GATEWAY_ALLOW_PUBLIC_HTTP=1`; that override is intended
only for a protected container/private network behind TLS termination.

## Publisher contract

After its authenticated WebSocket upgrade, a Desktop publisher sends a strict
`desktop_resume` hello. It then publishes either:

- `desktop_snapshot` with a canonical snapshot payload excluding `appId`,
  `streamNonce`, and `sequence` (the gateway owns those fields), or
- `desktop_idle` when no call is active.

The gateway accepts only one current Desktop publisher per app. Mobile clients
are admitted only after that publisher has asserted a `desktop_snapshot` or
`desktop_idle`, then receive canonical `snapshot` or `sync_ready` frames. A
Desktop replacement or disconnect starts a new stream epoch and fences every
mobile from the old authority. Commands are routed only over the live private
socket and acknowledgements return only to the device that originated the
matching command. A Desktop publisher must provide inbound WebSocket activity
(a valid message, ping, or pong) at least once every 45 seconds; expiry clears
its authority and fences its mobiles rather than leaving stale state visible.

Each validated command's `idempotencyKey` is bound to its device, command ID,
and complete command content. Exact retries follow the existing pending or
cached result, while key/identity mismatches fail closed without reaching the
Desktop. The in-memory idempotency index is bounded and evicted in step with
the acknowledgement cache.

This service intentionally does not expose a generic HTTP command endpoint.
