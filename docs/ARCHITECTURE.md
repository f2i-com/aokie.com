# Aokie architecture

Aokie is a desktop **plugin**, hosted by OAIY Desktop or FormLogic Desktop: a
JSON-RPC process (stdio, NDJSON) that owns the phone bridge and the call loop
and streams events to its host, which runs its flows and passes them on to
FormLogic, where records, roles, dashboards and flows live. This document maps
the moving parts and the two state machines that matter for correctness.

## Process shape

```
OAIY Desktop or FormLogic Desktop (Tauri host)
  └─ spawns  aokie-plugin.exe  (stdio JSON-RPC 2.0, NDJSON)
                ├─ radio thread            (WinUSB HCI/ACL/SCO, HFP, MAP, PBAP)
                ├─ STT worker               (Parakeet, generation-stamped jobs)
                ├─ outbox replay thread     (durable delivery, heartbeat)
                └─ (voice feature) in-plugin agent: STT → LLM → TTS
  └─ runs the services the plugin calls:
       OAIY Desktop       its gateway on 127.0.0.1:17872 (the realtime voice
                          route), OAIY Voice and its model engines
       FormLogic Desktop  aokie-voice-server.exe (:17920 loopback OpenAI-compatible
                          STT/TTS) and llama-server.exe (:8080 local LLM)
```

The plugin also ships its own screen, `crates/aokie-plugin/ui/receptionist`
(plain JavaScript, no build step). The host loads it into a sandboxed iframe
(opaque origin, CSP `default-src 'none'`) and injects `window.PluginHost`, a
postMessage bridge to the plugin's commands and events. OAIY's setup wizard
shows single tabs of the same screen as its steps (setup mode; see
[FORMLOGIC_PLUGIN_CONTRACT.md](FORMLOGIC_PLUGIN_CONTRACT.md#the-receptionist-screen-in-setup-mode)).

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

## Aokie's own voice pipeline (voice feature)

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

## The OAIY route (Desktop realtime)

With `realtimeVoiceMode=desktop_realtime` and the provider `oaiy`, the call's
audio streams to OAIY's voice gateway
(`ws://127.0.0.1:17872/api/ai/providers/oaiy/v1/realtime/stream`). OAIY Voice
hears and speaks, and OAIY's Front desk agent decides, with the saved persona
(or the call's `configureAgent` overlay) as its brief. No path counts on
Aokie's own speech stack, which is normally not downloaded on this route.

Aokie's fixed lines (a screened caller's message, the hold and queue
announcements, the mid-call failure apology) are said in OAIY's voice through
its speak mode: a `formlogic.realtime.start` with `"mode": "speak"` and the line
as its greeting, said once with no agent listening. Where OAIY cannot say a
line, Aokie's own TTS says it if present. Screened callers are never given to
OAIY's agent. A mid-call OAIY failure ends in a hangup and
`aokie.hardware.error {code: "realtime_failed", route: "oaiy", apologized,
apologizedWith?}`. The full rules are in
[FORMLOGIC_PLUGIN_CONTRACT.md](FORMLOGIC_PLUGIN_CONTRACT.md#calls-on-the-oaiy-route).

## Transferring a call to the owner (`transfer/`, OAIY route)

OAIY's call agent can hand the live call to the owner's own device. The wire
contract is `transfer_v1`, written down in
[contracts/transfer/transfer-v1.md](contracts/transfer/transfer-v1.md) with
fixtures that OAIY's repository shares byte for byte.

What the plugin offers OAIY:

- **A realtime tool**, `transfer_to_owner {reason}` (`caller_asked`, `urgent`,
  `policy_rule`), offered by `start.allowTransfer` and enabled for a session
  only if OAIY's `ready.features` lists `transfer_v1`. It is a tool on the
  loopback call stream, not a connector command, so it is never reachable from a
  website through the relay.
- **A result at once**: `ringing`, or a refusal with a closed reason and fixed
  text (`consent`, `pending_request`, `caller_did_not_ask`, the ring plan's
  reasons, `plan_unavailable`, the plugin's own ceilings).
- **A typed outcome later**, `formlogic.realtime.transfer_outcome`: `accepted`,
  `declined` (with the owner's bounded message), `unavailable`, `expired` or
  `cancelled`. A completed takeover sends none: the session stops with
  `handoff:takeover`, and the fresh session for the same call carries
  `start.resume` (`return` or `failback`).
- **A withdrawal from OAIY**, `formlogic.realtime.transfer_cancel {requestId,
  reason}` (`owner_declined`, `message_instead`, `gave_up`), handled like a
  request withdrawn because someone took the caller another way: a request
  nobody has won is withdrawn and reported once as `cancelled`; one an owner
  device has won is left alone and answered with a `transfer_notice`
  (`too_late`); an id the call has no open request for is answered
  `unknown_request`. Replay-safe, and only on a session that negotiated
  `transfer_v1`.

How it runs. The tool asks the host for a ring plan (`oaiy.ring.plan`, 1.5 s),
opens a request in the same volatile assistance mailbox that owner transfer on
Aokie's own route already used, aimed at the planned devices, and tells the host
(`oaiy.ring.opened`) so it can wake phones and toast the desktop. Nothing about
the media changed: the Companion gateway publishes signed offers, the first
accept wins the mailbox's compare-and-swap, and the existing v2 takeover path
bridges the caller to the accepting endpoint and pauses the AI. The transfer
machine (`transfer/call.rs`) lives in the call's context and is polled by the
radio loop each turn, so it outlives the realtime session that the takeover
stops; the per-call reset asks it to end an open request (reporting `cancelled`
and closing its audit event) before the context is replaced. The tool's answer
is queued and sent by the same drained-output gate as every other tool result.

| Ending | Sent to OAIY |
|---|---|
| An endpoint takes the caller | nothing; `stop handoff:takeover`, then a fresh session with `resume` |
| Declined | `declined`, with the owner's message if there is one |
| Nobody answered in the ring window | `expired` |
| Accepted, but the media setup failed or never completed | `unavailable` (the AI has the caller back) |
| Caller hung up, consent withdrawn, someone took the call another way | `cancelled` |

| Timing | Value |
|---|---|
| Wait for the host's plan | 1.5 s (no answer means nobody is rung) |
| Ring window | 20 to 90 s, the plan's (40 when it says nothing) |
| Media setup after an accept | 45 s, then 10 s for the gateway to record the result |
| The plugin's own deadline | 5 s past all of that, on the monotonic clock |

Rules that do not bend: consent must currently grant both `remote_assistance`
and `remote_takeover` (checked at the door, after the plan, and while ringing);
one request at a time; at most 3 a call, 15 s between one ending and the next, 3 an hour per caller number (2 for all withheld numbers together) and 20 an hour, on top of
the host's own policy; the request text sent to the owner's devices is fixed and
nothing the model or the caller says is relayed; a device the plan names is the
only kind offered the transfer (a plan that toasts the desktop and names no
device is `no_endpoint`: a toast is a notification, not a target, so for the
owner at the PC the plan names the Windows Companion by thumbprint, running or
not). A call that is not offered
the tool (any call before the host announces `ringPlan`, any older OAIY) behaves
exactly as before, including the text of its stop reason.

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
[FORMLOGIC_PLUGIN_CONTRACT.md](FORMLOGIC_PLUGIN_CONTRACT.md).
