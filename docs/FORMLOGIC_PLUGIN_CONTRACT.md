# Aokie → FormLogic Desktop Plugin — Obligations

**Canonical contracts:** `formlogic-app` repo → `docs/AOKIE_PLUGIN_CONTRACT.md` (this plugin's command/event surface), `docs/DESKTOP_PLUGIN_SDK.md` (host protocol), `docs/FORMLOGIC_DESKTOP.md`, `docs/ADR_FORMLOGIC_DESKTOP.md`. Local schema copies (validated by this repo's tests): `docs/contracts/*.schema.json`.

This repo owns the **Aokie Desktop Plugin**: the Bluetooth dongle / phone bridge (WinUSB, HFP/SCO, MAP SMS, PBAP contacts) packaged as a FormLogic Desktop plugin. Aokie's user experience moves to the "Aokie Receptionist for FormLogic" app package (formlogic-app repo); the legacy Tauri app remains a temporary developer/admin fallback and must keep compiling.

## Structure

```
crates/aokie-core     Tauri-free native logic (no `tauri::` imports — CI-enforced).
                      Gradual home for: aokie_radio, msbc, aokie_dongle(+catalog),
                      database, ai traits/adapters, redact, retention, ...
crates/aokie-plugin   The plugin process: main.rs, rpc.rs (JSON-RPC 2.0 NDJSON stdio),
                      connector.rs (command dispatch), event_bridge.rs, outbox.rs, config.rs
aokie-desktop/        Legacy Tauri app; re-exports moved modules from aokie-core.
```

## Must implement (MVP)

- Manifest `plugins/aokie/manifest.json` per `plugin-manifest.schema.json` (id `aokie`, `pluginApiVersion:1`).
- Protocol: `plugin.init` (≤10 s), `plugin.health`, `plugin.shutdown`, `connector.request`; notifications `event.emit` (envelope per `desktop-event.schema.json`, `source:"aokie"`, stable `idempotencyKey = aokie:<correlationId>:<step>:v1`) and `log.emit` (redacted).
- MVP commands: `dongle.list/getPreferred/setPreferred/installDriver/diagnostics`, `phone.status/startPairing/stopPairing/listPaired`, `call.current/answer/reject/hangup/operatorSpeak`, `sms.threads/thread/send`, `settings.get/set`. Hardware-dependent commands may return typed `command_failed` until wired; never fake success.
- Dev/mock mode (`FORMLOGIC_DEV_MODE=1`): scripted call lifecycle events for integration tests.
- Local SQLite outbox (`aokie_outbox`, UNIQUE idempotency_key) written before emitting essential raw-record events; retry with backoff; `dead` surfaced via diagnostics.
- Never write non-protocol output to stdout; PII redacted in logs by default.
