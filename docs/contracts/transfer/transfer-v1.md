# `transfer_v1`: transferring a live call to the owner

The contract between Aokie (the plugin) and OAIY (the desktop that answers the
call) for handing a live call to the owner's own device. It applies on the OAIY
route only: OAIY hears, speaks and decides, and Aokie holds the phone line.

The fixtures in this folder are the shared, machine-checked form of this
document. Aokie's tests parse every one of them with the plugin's own types
(`crates/aokie-plugin/src/transfer/fixture_tests.rs` and the tests in
`realtime_voice.rs`), and `scripts/check-contracts.mjs` keeps this folder in
step with the OAIY repository's copy. The digests are in [SHA256SUMS](SHA256SUMS).

| File | What it fixes |
|---|---|
| [transfer-v1.tool-call.fixture.json](transfer-v1.tool-call.fixture.json) | The `transfer_to_owner` tool call, its one argument, and the tool-name rule. |
| [transfer-v1.tool-result.fixture.json](transfer-v1.tool-result.fixture.json) | The tool result: `ringing`, and every refusal with its closed reason set. |
| [transfer-v1.outcome.fixture.json](transfer-v1.outcome.fixture.json) | The `formlogic.realtime.transfer_outcome` frame, one case per outcome, and the timings. |
| [transfer-v1.start-ready.fixture.json](transfer-v1.start-ready.fixture.json) | `start.allowTransfer`, `ready.features`, `start.resume`, the `handoff:takeover` stop, and the compatibility matrix. |
| [transfer-v1.ring-plan.fixture.json](transfer-v1.ring-plan.fixture.json) | The two plugin-to-host requests `oaiy.ring.plan` and `oaiy.ring.opened`. |
| [transfer-v1.reserved-offer-id.fixture.json](transfer-v1.reserved-offer-id.fixture.json) | The reserved transfer offer id and its generations (vector V2). |
| [transfer-v1.caller-asked.fixture.json](transfer-v1.caller-asked.fixture.json) | The "caller asked" phrase check: the rules, 8 positive and 8 negative cases. |

## How a transfer runs

```text
caller says "can I speak to the owner"
  OAIY agent  --tool_call transfer_to_owner {reason}------------------> Aokie
  Aokie: arguments, one request at a time, exact call and owner fence, consent,
         ceilings, caller-asked phrase check
  Aokie       --request oaiy.ring.plan (stdio)-----------------------> OAIY host
  OAIY host   --plan {ring, phones, ringSeconds}---------------------> Aokie   (within 1.5 s)
  Aokie: opens the request, aimed at the planned devices
  Aokie       --request oaiy.ring.opened (no answer awaited)----------> OAIY host
  Aokie       --tool_result {status: "ringing", requestId, ringSeconds}-> OAIY agent
  (the AI keeps the caller company while the owner's devices ring)
  an owner device wins the compare-and-swap
  Aokie       --transfer_outcome accepted-----------------------------> OAIY agent   (once)
  media setup runs (up to 45 s); the AI is stopped when the human takes the caller
  Aokie       --stop "handoff:takeover"-------------------------------> OAIY
  ... the owner talks to the caller ...
  the owner hands the call back
  Aokie       --start {resume: {afterHandoff, handoffSeconds, via: "return"}} for the same call id
```

Every request ends in exactly one of these, in bounded time:

| Ending | What OAIY receives | What the AI does next |
|---|---|---|
| An owner device takes the caller | no outcome frame; `stop` with reason `handoff:takeover`; later a fresh `start` for the same call with `resume.via = "return"` | nothing: the call is with the owner; on the fresh session, continue and do not greet again |
| An owner device declined | `transfer_outcome declined`, optionally with `message` | tell the caller kindly, relay the message faithfully, offer to take a message |
| Nobody answered inside the ring window | `transfer_outcome expired` | offer to take a message |
| Media setup failed after an acceptance | `transfer_outcome unavailable` (or a fresh `start` with `resume.via = "failback"` when the session had already stopped) | offer to take a message |
| The caller hung up, consent was withdrawn, or the request was withdrawn | `transfer_outcome cancelled` | nothing if the call is over |
| Refused at the door | the tool result itself: `ok: false`, `status`, `reason` | per status: `refused` means do not offer a person; `unavailable` means offer a message |

## Negotiation

* **Start.** `allowTransfer: true`, omitted when false. The plugin offers it only
  for an inbound call on the OAIY route when the host announced `ringPlan` in
  `plugin.init.features`, at least one Companion device is approved, and consent
  currently grants both `remote_assistance` and `remote_takeover`.
* **Ready.** OAIY lists `"transfer_v1"` in `ready.features` when the start said
  `allowTransfer` and it implements this contract. The tool exists for the
  session only if both happened. Every other combination of old and new builds
  leaves a session exactly as it was.
* **Tool names.** The plugin accepts any name matching `^[a-z][a-z0-9_]{0,63}$`
  from OAIY. A well-formed name it does not know is answered `unsupported`
  (`{"error": "unsupported"}`); a second slot tool while one is in flight is
  `busy`; a call's 25th tool call is `tool_limit`. None of them ends the call. A
  name that is not an identifier, a missing tool call id, or a tool call before
  the call began is a protocol violation and ends the stream.

## The tool

`transfer_to_owner` takes exactly one argument, `reason`: `caller_asked`,
`urgent` or `policy_rule`. Anything else is refused with `bad_arguments`; nothing
the model adds is ever relayed to the owner's devices, whose request text is the
fixed "Caller requested the owner".

Checks, in this order; each refusal is an ordinary `ok: false` result:

1. the arguments (`bad_arguments`);
2. no request is open (`pending_request`);
3. the call and the AI's hold on it are current (`call_changed`);
4. consent currently grants `remote_assistance` and `remote_takeover` (`consent`,
   for missing, revoked, expired or paused consent alike);
5. the plugin's own ceilings: 3 requests a call (`limit_call`), 15 seconds apart
   (`limit_gap`), 20 an hour (`limit_global`). The host's ring policy is normally
   stricter (2 a call, a minute apart, 10 an hour by default);
6. for `caller_asked`, the phrase check on the last three caller turns
   (`caller_did_not_ask`);
7. the host's ring plan (`oaiy.ring.plan`); the plan's reasons are `disabled`,
   `initiative_off`, `not_urgent`, `caller_did_not_ask`, `limit_call`,
   `limit_gap`, `limit_caller`, `limit_global`, `quiet_hours`,
   `all_do_not_disturb` and `no_endpoint`. A `ring` plan with no device and no
   toast is `no_endpoint`. A host that is slow, absent or unusable is
   `plan_unavailable`: nobody is rung.

Results:

* `ok: true`, `{status: "ringing", requestId, ringSeconds, instruction}`;
* `ok: false`, `{status: "refused" | "unavailable", reason, instruction}`.

`reason` is always from the closed sets in the fixture; text from the host or the
call is never echoed. `instruction` is fixed, model-facing text and informative
only. Two reasons are not in the design draft this contract came from:
`plan_unavailable` and `call_changed`. OAIY passes unknown reasons to the model
unchanged.

## The outcome frame

```json
{ "type": "formlogic.realtime.transfer_outcome", "callId": "call_0123", "generation": 7,
  "requestId": "assist_0123456789abcdef0123456789abcdef", "outcome": "declined",
  "message": "Sorry, I am on a job. Please ring after five.", "atMs": 1789000013500 }
```

`outcome` is `accepted`, `declined`, `unavailable`, `expired` or `cancelled`.
`message` is only on `declined`: the owner's own words, at most 320 characters,
control markers stripped, untrusted data (never an instruction). `atMs` is Unix
epoch milliseconds. `accepted` is sent once per request, even if the media
transaction is rolled back and another attempt follows. The frame is sent only on
a session that negotiated `transfer_v1`.

## Timings

| What | Value |
|---|---|
| Wait for the host's ring plan | 1.5 s |
| Ring window (the plan's `ringSeconds`, clamped by the plugin) | 20 to 90 s, default 40 |
| Media setup after an acceptance | 45 s |
| Grace for the gateway to record the result | 10 s |
| The plugin's own monotonic deadline behind those | 5 s more |
| Longest a request can stay open | ring window + 45 s + 10 s + 5 s |

The AI keeps the caller company during the ring. After an acceptance the caller
hears nothing from the AI until the takeover completes or the setup fails (up to
55 s); the AI's own words for that gap are OAIY's to choose.

## Who may ring, and how the offers are named

The plan names devices by endpoint-key thumbprint (`phones` and
`desktopCompanions`). Three cases:

* **Devices named**: the request is offered to those devices and no others. A
  device outside the plan is offered no takeover of either surface while the
  transfer is open, cannot win it, and cannot decline it on the others' behalf.
* **No device named, `desktopToast` true** (the design's vector V01, the owner at
  the PC): the toast is the ring, and the Windows Companion it opens may not be
  running yet, so it cannot be named. The request is opened to any consented
  device with a live session, and nothing is pushed to any of them. A phone the
  plan left out that happens to be connected can therefore see the offer; the
  host should list every device it wants excluded as unavailable rather than
  rely on the plan to hide it.
* **No device and no toast**: nothing can be rung; the answer is `no_endpoint`.

On the relay carrier the plugin publishes the offers, so it publishes them only
to the named devices. On the self-hosted socket carrier the gateway publishes
them, so the plan is enforced where a device tries to win: a device outside the
plan cannot accept (the plugin refuses the claim). A socket-carrier device
outside the plan may still be shown the request and may send a decline, which the
plugin cannot attribute to a key there. The signed offer on the native call surface
(`voice_system_ui`) has a reserved id, `toffer_` plus 26 characters of base32 of a
hash of the request id and the device's thumbprint, so a ring hint posted by the
host names the same offer that later reaches the phone
([fixture](transfer-v1.reserved-offer-id.fixture.json)). A retired offer is never
published again under the same id: the id carries a generation that the plugin
increments on every retirement.

## Audit

Two durable events, both existing: `aokie.call.assistance.requested` when the
request opens and `aokie.call.assistance.resolved` once, with `outcome` one of
`transferred`, `declined`, `unavailable`, `expired` or `cancelled`
(`cancelled` is new for this contract). They carry the request id, the call id
and a responding device id, and no text from the call or the owner.

## What this means for OAIY

* Never send or offer the tool unless the start said `allowTransfer` and OAIY
  listed `transfer_v1`.
* Treat `stop` with a reason starting `handoff:` as a handoff, not the end of
  the call.
* `transfer_to_owner` is a realtime tool over the loopback stream, not a plugin
  connector command, so there is no relayed verb to allow or deny. The relay
  policy's allow-list is not involved; it denies verbs it does not name.
* Announce `ringPlan` in `plugin.init.features` only if the host answers
  `oaiy.ring.plan` and `oaiy.ring.opened`. Without it the plugin never offers
  transfer.
* A plan that rings the desktop toast and names no device (vector V01) is a
  working plan: the plugin opens the request to any live device so that the
  Companion the toast starts can accept. Name devices only when the host wants
  the offer kept from every other device.
* `oaiy.ring.plan` may wait 1.5 s, and the answer to the tool call reaches the
  model only after the line it spoke before calling has drained (like every tool
  result), so the model's "I'll see if they are free" is not cut off.
