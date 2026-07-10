# Aokie — AI Phone Receptionist Bridge

Aokie turns a Windows PC, a Bluetooth dongle, and a paired mobile phone into
an AI phone receptionist. It answers calls over HFP/SCO, transcribes callers
locally (Parakeet), replies with a local LLM + TTS (pocket-tts), receives SMS
over MAP, and streams everything to [FormLogic](https://github.com/f2i-com/formlogic.com)
as a **desktop plugin** — business records, dashboards, roles, and flows live
there, behind a versioned stdio contract.

Aokie is a plugin, not a platform: it owns the radio and the voice loop;
FormLogic owns policy, records, and UI.

## Workspace

| Crate | What it is |
|---|---|
| `aokie-plugin` | The FormLogic Desktop plugin process (JSON-RPC over stdio): connector commands, event outbox (durable, DPAPI-protected), call-session state machine, in-plugin voice agent. |
| `aokie-bluetooth` | WinUSB HCI/ACL/SCO radio runtime: HFP, mSBC/CVSD audio, MAP SMS, PBAP contacts, recovery logic. |
| `aokie-dongle` | Dongle discovery, WinUSB driver installation (elevated helper), event mapping. |
| `aokie-ai` | Local STT/TTS runtimes (ONNX). |
| `aokie-voice-server` | Loopback OpenAI-compatible STT/TTS HTTP service (`:17920`). |
| `aokie-core`, `aokie-audio`, `aokie-db`, `aokie-receptionist` | Shared native logic. |

## Build & test

Windows only (the radio is WinUSB-based).

```bash
cargo test --workspace                          # the full gate CI runs
cargo check -p aokie-plugin --features voice    # the voice cfg surface — always check BOTH
cargo clippy --workspace --all-targets          # deny-level lints block CI

# Release plugin build (the elevation gate pins the helper hash):
export AOKIE_EXPECTED_HELPER_SHA256=<sha256 of the deployed aokie-driver-helper.exe>
cargo build -p aokie-plugin --features voice --release
```

## Docs

- `docs/ARCHITECTURE.md` — process shape, the call-session + voice state
  machines, and the durability model.
- `docs/HARDWARE.md` — supported dongle chipsets (compatibility tiers) and
  the Windows/phone notes.
- `docs/FORMLOGIC_PLUGIN_CONTRACT.md` + `docs/contracts/*.json` — the frozen
  cross-repo surface (events, commands, errors, settings schema, persona).

## Contract

The FormLogic↔Aokie surface (events, commands, errors, settings schema,
persona) is frozen in `docs/contracts/*.json` — byte-identical copies live in
the FormLogic repo, and each repo's tests lock its own artifacts against its
copy. Changing the contract is a coordinated two-repo change by construction.
Operator-facing runbook + troubleshooting live in the FormLogic repo
(`docs/AOKIE_OPERATIONS.md`, `docs/AOKIE_TROUBLESHOOTING.md`).

## Security

See [SECURITY.md](SECURITY.md). Highlights: outbox payloads (transcripts,
SMS) are DPAPI-protected at rest; conversation content never appears in logs
unless `AOKIE_LOG_CONTENT=1`; speech endpoints are URL-classified before use;
`autoAnswer` defaults **off**.
