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
- Occurrence identity (AOK-EVENT-001): a step that can REPEAT under one correlation (radio lifecycle under `radio` — `dongle.ready`, `phone.connected/disconnected`, `hardware.error` — and per-call `audio.connected/disconnected`) appends an immutable per-incident occurrence id to the step (`aokie:radio:hardware.error.<occ12>:v1`), minted once at detection so replays keep one key while every new incident gets its own. Inbound SMS identity is `sms_<deviceAddr>_<mapHandle>` (stable per phone+message; re-fetches dedupe); a phone that sends no handle falls back to a fresh occurrence id.
- MVP commands: `dongle.list/getPreferred/setPreferred/installDriver/diagnostics`, `phone.status/startPairing/stopPairing/confirmPairing/listPaired/removePaired`, `call.current/answer/reject/hangup/operatorSpeak`, `sms.threads/thread/send`, `settings.get/set`. Hardware-dependent commands may return typed `command_failed` until wired; never fake success.
- Pairing security (PAIR-001): SSP runs as NUMERIC COMPARISON — the radio advertises DisplayYesNo+MITM and HOLDS the confirmation (`phone.status.pairingConfirm` + the `aokie.phone.pairing_confirm_required` event) until the operator answers `phone.confirmPairing {address, accept}` (auto-refused after ~25s). Legacy fixed-PIN (0000) pairing is OFF unless the `legacyPairingPin` setting is explicitly on. Link keys are DPAPI-sealed at rest (pairing store v2; v1 plaintext migrates on load). `phone.removePaired` disconnects an active session for that device before dropping its key.
- Dev/mock mode (`FORMLOGIC_DEV_MODE=1`): scripted call lifecycle events for integration tests.
- Local SQLite outbox (`aokie_outbox`, UNIQUE idempotency_key) written before emitting essential raw-record events; retry with backoff; `dead` surfaced via diagnostics. `sent` is terminal (ack/replay races can never move it backward; bookkeeping is generation-conditional), and a same-key/different-content insert is REJECTED and counted (`dongle.diagnostics → outbox.keyCollisions`) — it means a key-derivation bug, never a harmless duplicate.
- Never write non-protocol output to stdout; PII redacted in logs by default.
