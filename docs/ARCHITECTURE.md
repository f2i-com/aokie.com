# Aokie — Architecture

Aokie is a FormLogic Desktop **plugin**: a JSON-RPC process (stdio, NDJSON)
that owns the phone bridge and the voice loop and streams events to FormLogic,
which owns records, roles, dashboards and flows. This document maps the moving
parts and the two state machines that matter for correctness.

## Process shape

```
FormLogic Desktop (Tauri host)
  └─ spawns  aokie-plugin.exe  (stdio JSON-RPC 2.0, NDJSON)
                ├─ radio thread            (WinUSB HCI/ACL/SCO, HFP, MAP, PBAP)
                ├─ STT worker               (Parakeet, generation-stamped jobs)
                ├─ outbox replay thread     (durable delivery, heartbeat)
                └─ (voice feature) in-plugin agent: STT → LLM → TTS
  └─ spawns  aokie-voice-server.exe  (:17920 loopback OpenAI-compatible STT/TTS)
  └─ spawns  llama-server.exe        (:8080 local LLM the agent reuses)
```

The plugin inherits no secrets — only an allow-listed environment. Everything
it persists lives under the per-plugin data dir handed to it at `plugin.init`.

## Call-session state machine (`call_session.rs`)

One live call is a `CallSession` with an **immutable** id (`call_<uuid>`, never
reused) and a monotonic **generation** stamped through the async voice
pipeline. The generation is how a slow STT result from call A is dropped rather
than attributed to call B.

```
        CallIncoming            CallAnswered           CallTerminated
   idle ───────────▶ Ringing ───────────▶ Active ───────────▶ (consumed)
                        │                                   ▲
                        └──────── CallTerminated ───────────┘
```

Termination is computed from `(answered?, intent)` into an explicit outcome —
never a guess:

| answered | intent | outcome | reason |
|---|---|---|---|
| yes | OperatorHangup | completed | operator_hangup |
| yes | (remote) | completed | remote_or_operator |
| yes | DeviceLost | completed | device_lost |
| no | OperatorReject | rejected | operator_reject |
| no | DeviceLost | missed | device_lost |
| no | (none) | missed | remote_or_operator |

Key invariants (each pinned by a test):

- **`incoming` always precedes** `answered`/`ended`/audio events, even during
  the ~800 ms caller-ID enrichment hold — every call-scoped emission
  force-flushes a pending `incoming` first.
- **Device loss terminates the call exactly once**: a dropped dongle
  synthesizes termination through the same machine (`reason: device_lost`); a
  late real `CallTerminated` lands on an idle tracker and is a no-op.
- **The caller's final on-device STT lands before `call.ended`**: on
  termination the radio drains buffered audio + in-flight STT (bounded 1.5 s).
  Optional detached audio-model corrections may arrive later, so transcript-
  consuming background flows use `call.transcript.settled`, emitted after all
  correction workers report or the bounded 40 s correction deadline. The
  ordinary `call.ended` event stays immediate for lifecycle/UI/callback work.
  Graceful plugin/consent shutdown forces any pending barrier with
  `transcriptCorrectionTimedOut: true` and waits for its synchronous outbox
  write before acknowledging shutdown. The correction ledger itself is still
  process-memory state: an abrupt process/OS/power failure after durable
  `call.ended` but before the barrier can omit that call's background analysis.
  The existing outbox cannot safely hold a not-yet-deliverable event (pending
  rows replay immediately), so crash recovery needs a dedicated persistent
  delayed-work ledger rather than reusing the delivery outbox.

## Voice pipeline (voice feature)

Per caller turn: energy-VAD segments speech → generation-stamped STT job →
stream the local LLM (first sentence starts TTS immediately) → per-sentence
TTS → SCO out. Barge-in: AEC cancels Aokie's own voice from the mic; sustained
caller speech flushes the queued TTS tail and aborts the LLM.

Truthfulness rules the transcript: a barged or errored reply records only the
sentences that **actually played** (`[caller interrupted]` / `[reply cut short
by an error]`), and an `operatorSpeak` that produced no audio records nothing.

`AudioConnected` carries `armed` — false means the SCO alternate-setting failed
and the call is silent both ways; the plugin emits `hardware.error
{sco_unarmed}` with a recovery action rather than reporting a healthy call.

## Durability (`outbox.rs`)

Every essential record event (`call.*`, `sms.*`, `hardware.error`) is written
to a SQLite outbox **before** emission (`synchronous=FULL`). Delivery is
ACK-gated: the desktop journals the envelope (fsynced) before sending
`event.ack`; unacked rows stay pending and a replay thread re-delivers on
backoff, dead-lettering after the attempt budget. Payloads (transcripts, SMS)
are **DPAPI-protected at rest with no plaintext fallback** (AOK-DUR-001): a
protect failure QUARANTINES the event as a typed dead row (metadata kept,
payload absent, never emitted or redriven), a decrypt failure dead-letters as
`payload_unreadable` instead of emitting an empty replacement, and legacy
plaintext rows are sealed in place at open (verified per row, then vacuumed so
no plaintext pages survive). A host without `eventAck` is treated as
incompatible in production: essential events are HELD in the outbox and
`plugin.health` degrades, unless `AOKIE_ALLOW_LEGACY_HOST=1` explicitly
accepts write-means-sent delivery (non-Windows dev builds likewise need
`AOKIE_ALLOW_UNPROTECTED_OUTBOX=1` to store plaintext). Dead rows expire after
14 days; an operator can redrive one (`outbox.redrive {idempotencyKey}`) or
all (`{all:true}`) — undeliverable (quarantined/unreadable) rows are excluded.
The replay thread's heartbeat is surfaced in `plugin.health`, so a frozen
delivery pipeline degrades readiness rather than silently stalling.

## Contract

Events, commands, error codes, the settings schema and the persona are frozen
in `docs/contracts/*.json`, byte-identical in the FormLogic repo, and each
repo's tests lock its own artifacts against its copy — see
`docs/FORMLOGIC_PLUGIN_CONTRACT.md`.
