# Native Bluetooth Transport Plan — Using Windows' Built-in Stack Instead of WinUSB

**Status:** IMPLEMENTED + FIELD-EVALUATED (2026-07-20). Native mode is live and
proven for SMS / contacts / pairing / call control with zero driver install.
**Native call AUDIO is impossible on Windows 11 25H2** — see the findings
below; `transportMode` defaults to `dongle` and the UI no longer offers the
native option.

## 2026-07-20 field findings (why native call audio is a dead end on 25H2)

Verified live on the target box (Pixel 9a, BCM20702 dongle and MediaTek MT7922
radios, Windows 11 Pro 25H2 build 26200.8655, MediaTek driver 1.1146.0.576):

- **Windows 11 25H2 does not ship `BthHFSrv.dll`** — the classic HFP
  hands-free-unit service is absent from `System32` AND the WinSxS component
  store, so it cannot be re-enabled or repaired. The HFP *drivers*
  (`BthHfEnum.sys`, `BthHfAud.sys`) remain; the service that ran the SLC and
  exposed the "Bluetooth Hands-Free" WASAPI endpoints is gone. (The
  `ProductName = "Windows 10 Pro"` registry value is a known stock quirk, not
  a custom-image signal.)
- **No telephony audio endpoints ever appear** — probed every render/capture
  MMDevice endpoint repeatedly, including mid-call on both radios: only the
  default PC devices exist. Even Microsoft's own Phone Link dialer cannot get
  Bluetooth call audio on this build.
- **LE Audio telephony does not engage** — the "Use LE Audio when available"
  Settings toggle never appears even on the 2026 MediaTek driver branch
  (1.1146.0.576), and no LE-Audio endpoint appears. SCO audio is kernel-only
  (verified: Winsock Bluetooth sockets support only `BTHPROTO_RFCOMM`; all
  SCO DDIs are kernel-mode profile-driver interfaces), so a user-mode app
  cannot implement HFP-HF with audio either.
- **Phone Link's call audio is a NETWORK channel**, not Bluetooth — the
  Link-to-Windows app streams call audio over LAN/WiFi to the PC (no
  Bluetooth audio device exists in any state during its calls). That channel
  is proprietary to Microsoft's apps; the Calls API exposes control only
  (status/answer/end/dial), never audio. Android also blocks third-party apps
  from capturing cellular call audio (downlink needs system privileges), so a
  self-built phone-side bridge is out too.
- **Consequence:** the WinUSB userspace host stack (this repo) is the ONLY
  working Bluetooth phone-bridge with audio on Windows 11 25H2. The native
  backend (`aokie-winbt`) remains valuable for driverless SMS/contacts/call
  control and for older Windows builds where `BthHFSrv` exists.
- Incidental fixes shipped during the evaluation: the desktop's plugin-spawn
  `env_clear` was stripping `AOKIE_ALLOW_SELF_SIGNED_DRIVER` (driver
  self-signing silently degraded to refused production jobs — fixed in the
  spawn allow-list), and the package-signer now skips `%VAR%`-shaped paths
  (a literal `%SystemDrive%` cert-cache copy kept being written into the
  plugin dir and broke verification on reboot).

**Original status note (2026-07-19):** IMPLEMENTED (v1, awaiting live hardware test).
Phase 0 spike passed (unpackaged `PhoneLineWatcher` verified on the live box).
Phase 1 (the `RadioBackend` seam + `UsbRadioBackend`) and Phase 2 (new crate
`crates/aokie-winbt`: Calls-API engine, WASAPI Hands-Free pump, MAP/PBAP over
WinRT RFCOMM, pairing helpers, worker integration) are complete and gated
green. Selection rides the new `transportMode` setting (`dongle` default /
`native` / `auto`). Remaining: the supervised live-call test with a
Windows-paired phone (§6 Phase 4), Device-Setup UI polish for native pairing,
and the Phone Line conflict check.

**Original scope:** `aokie-plugin` + `aokie-bluetooth` + `aokie-dongle` (this repo), with knock-on
changes in FormLogic Desktop and the Aokie Receptionist pack UI.

---

## TL;DR — recommendation

Yes, it's possible — with one hard caveat that shapes everything: **Windows gives user-mode
apps no way to touch raw SCO/eSCO audio or raw HFP AT commands.** Call audio on the built-in
stack is only reachable through the "Bluetooth Hands-Free" WASAPI endpoints, which means
Windows' own HFP service owns the call, and our carefully built AT-command layer
(`ATA`/`CHUP`/`ATD`/`CLCC`/`CHLD`/`CIND`) cannot be used.

So the honest design is a **dual-mode transport**:

- **Native mode (new, default):** zero driver install, uses the PC's existing Bluetooth
  (built-in or any dongle with stock drivers). Call control via the WinRT Calls APIs,
  audio via WASAPI, SMS via MAP-over-RFCOMM (which we keep — Windows lets us open RFCOMM
  channels to the phone's MAP/PBAP services). Covers the whole core product: answer, talk,
  hang up, dial, caller ID, screening, SMS follow-ups, Companion takeover.
- **Dongle mode (existing, advanced):** today's WinUSB stack, kept as the "full control"
  mode for the features Windows cannot expose — above all the **call-waiting switchboard
  / hold queue (CHLD juggling)** — and as the only path on Linux.

The one go/no-go unknown is whether the WinRT Calls APIs (`PhoneLineWatcher`) work from
our unpackaged Tauri process. That is a **1–2 day spike and it must happen first**
(Phase 0 below). Everything else in this document is verified feasible on paper.

---

## 1. Why do this

Today's onboarding requires the user to bind their Bluetooth dongle to a WinUSB driver via
our elevated driver helper (`aokie-driver-helper.exe`, signed job files, INF/CAT package,
`UpdateDriverForPlugAndPlayDevicesW`). That:

- **Hijacks the dongle from Windows** — it stops working as a normal Bluetooth radio for
  everything else on the PC.
- Needs UAC elevation, driver signing (self-signed root CA gating), a dongle compatibility
  catalog, and reenumeration quirks (`reenumerateHwid`, selective-suspend registry fixes,
  physical-replug recovery for wedged controllers).
- Is the single biggest support/maintenance surface we have: `crates/aokie-dongle`
  (~6.5k lines of driver/PKI/installer code) plus the USB-corruption watchdogs, iso
  alt-setting management, and pacing logic inside `aokie-bluetooth` (~37k lines).

A laptop with built-in Bluetooth — or any dongle with stock Windows drivers — should "just
work". That's the ask, and it's a good one.

## 2. Current architecture (what we're replacing)

Replacement boundary analysis from the code:

- The plugin talks to exactly **one concrete façade**:
  `aokie_dongle::bluetooth::BluetoothManager` (`crates/aokie-dongle/src/bluetooth/mod.rs:140`)
  wrapping `AokieRuntime` (`crates/aokie-bluetooth/src/aokie_radio/runtime.rs:466`).
  Its API is channel-shaped: `control_tx` (ControlCommand in), `event_rx` (RuntimeEvent out),
  `audio_rx` (16 kHz PCM out), `status` (Arc atomics).
- The plugin's voice pipeline (`crates/aokie-plugin/src/radio.rs`, ~18k lines — STT/TTS/LLM/
  agent/duplex/speech planner/screening/switchboard) is **transport-ignorant by capability
  but coupled by concrete type**: ~13 functions take `bt: &mut BluetoothManager`, ~111 call
  sites. Nothing in `run_loop` knows about USB/HCI — it consumes `BluetoothEvent`s and PCM
  and calls verbs (`answer_call`, `send_audio`, `hangup`).
- Below the waist, `aokie_radio/transport.rs` is a **compile-time cfg facade**, not a trait:
  `AokieHciTransport` (WinUSB on Windows, libusb on Linux) is opened concretely by
  `run_runtime`. The whole pre-SCO protocol stack above it (HCI parsers, L2CAP, RFCOMM,
  SDP, HFP, OBEX, MAP, PBAP — all portable Rust) is fed by ~8 inherent methods on that
  transport.
- Existing test seams that prove the plugin side is mockable: `AudioLink` trait +
  `FakeLink` (`radio.rs:3250`, synthetic-audio rig with fake clocks), and the connector
  dev-mode mock radio (`mockCalls`).

**Conclusion:** the natural seam for a second transport is the `BluetoothManager`/
`AokieRuntime` channel API. Everything the plugin does above that line survives unchanged.

## 3. The hard constraint (verified)

| Question | Answer (verified on Microsoft Learn) |
|---|---|
| Can user-mode read/write raw SCO/eSCO? | **No.** Winsock Bluetooth sockets only support `BTHPROTO_RFCOMM`; all SCO interfaces are kernel-mode profile-driver DDIs. [bluetooth-and-socket](https://learn.microsoft.com/en-us/windows/win32/bluetooth/bluetooth-and-socket), [profile drivers](https://learn.microsoft.com/en-us/windows-hardware/drivers/bluetooth/bluetooth-profile-drivers-overview) |
| Can an app send custom AT commands while Windows HFP is connected? | **No supported path.** Windows' built-in HFP service owns the phone's AG channel and the SCO audio path. Disabling "Hands-free Telephony" per device removes the audio endpoints too — you get nothing. |
| How do you get call audio then? | WASAPI "Bluetooth Hands-Free" render + capture endpoints. Opening/streaming them makes Windows open the SCO/eSCO link on demand. Wideband (mSBC, 16 kHz) auto-negotiated since **Windows 10 1703**; no app-level codec control. [bluetooth-classic-audio](https://learn.microsoft.com/en-us/windows-hardware/drivers/bluetooth/bluetooth-classic-audio) |
| Can we still do RFCOMM to the phone's other services? | **Yes.** `Windows.Devices.Bluetooth.Rfcomm` works from unpackaged desktop apps: connect to any short-UUID service on a paired phone (MAP MAS `0x1132`, PBAP PSE `0x112F` is even predefined), read raw SDP attributes, and **host** services via `RfcommServiceProvider` + custom SDP record (MAP MNS `0x1133`). Only the `bluetooth` capability. [RfcommDeviceService](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.rfcomm.rfcommdeviceservice), [RfcommServiceProvider](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.rfcomm.rfcommserviceprovider) |
| Call control surface? | WinRT `Windows.ApplicationModel.Calls`: `PhoneLineTransportDevice` (Bluetooth-only, Win10 1903+), `PhoneLineWatcher`, `PhoneCall` (`AcceptIncoming`/`RejectIncoming`/`End`/`Hold`/`ResumeFromHold`/`Mute`/`SendDtmfKey`). Watching requires **restricted** capabilities (`phoneCallHistory` + `phoneCallHistorySystem`); registration requires restricted `phoneLineTransportManagement`. Restricted capabilities are **sideloadable** without Store approval. TAPI: **no documented Bluetooth HFP line** on Win10/11 (dead end — the old DUN/modem path doesn't apply). [PhoneLineTransportDevice](https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.calls.phonelinetransportdevice), [PhoneCall](https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.calls.phonecall), [capabilities](https://learn.microsoft.com/en-us/windows/uwp/packaging/app-capability-declarations) |
| Pairing? | `DeviceInformation.Pairing` custom pairing (system consent dialog, numeric comparison handled by Windows); HFP endpoints are created as part of pairing. Basic `PairAsync` is desktop-blocked, custom pairing is the path. [pair-devices](https://learn.microsoft.com/en-us/windows/apps/develop/devices-sensors/pair-devices) |
| Windows version baseline | Windows 11 22H2 documents Bluetooth 5.3 + **HFP 1.7.2**. WBS audio since Win10 1703. |

**Consequences:** because Windows owns HFP in native mode, our entire AT layer
(`hfp.rs`/`hfp_client.rs`, CLCC topology judges, CIND parsing, held-verdict machinery,
CHLD) is bypassed, and with it the **switchboard/auto-hold queue** in its current form.

## 4. Feature impact matrix

### Works in native mode (adapted mechanism, same product behavior)

| Feature | Today | Native-mode mechanism |
|---|---|---|
| Auto-answer + personalization window | `ATA` after CLIP/overlay hold | `PhoneCall.AcceptIncoming`; caller ID is present at offering, so the personalize flow + greeting hold works the same |
| Caller ID (+CLIP / CLCC rescue / withheld) | AT layer | `PhoneCall`/line caller id; withheld = empty |
| Hangup / reject (agent `[[END_CALL]]`, screening, abuse, fail-safes) | `AT+CHUP` | `PhoneCall.End` / `RejectIncoming` |
| Outbound dial with guardrails | `ATD` | `PhoneLine.Dial` (⚠️ docs note a foreground requirement — spike item; quiet-hours/ledger/consent guardrails are plugin-side and unchanged) |
| DTMF | not implemented today | `PhoneCall.SendDtmfKey` — actually a free upgrade |
| Call screening (blocked/private/pattern, screen messages) | plugin policy over caller id | unchanged — same caller-id source |
| SMS send/receive | MAP MAS over our RFCOMM mux | MAP MAS over WinRT RFCOMM — **reuse `obex.rs`, `map_mas.rs`, `bmessage.rs`, `map_listing.rs`** with a StreamSocket transport adapter; MNS via `RfcommServiceProvider` or keep the proven 45 s PollInbox |
| PBAP phonebook pull | OBEX/PBAP | same reuse — `pbap.rs`, `vcard.rs` over RFCOMM |
| Voice pipeline (STT/TTS/LLM/agent/duplex/speech planner) | SCO PCM | WASAPI capture/render PCM, resampled to 16 kHz mono — the pipeline already handles 8/16 kHz |
| Barge-in + AEC | speexdsp over SCO RX with TTS reference | same speexdsp; reference = TTS PCM sent to the render stream, capture = HF mic stream. ⚠️ Latency budget is bigger through WASAPI — spike must validate barge feel |
| Companion takeover (remote_media) | taps SCO PCM | unchanged tap point at the PCM seam |
| Contract events (`aokie.call.*`, `aokie.sms.*`, health) | AT-driven | same event names/payloads, driven by Calls/MAP events — **FormLogic flows and packs untouched** |
| Self-test, screening, abuse handling, manager line, SMS loop, callbacks | plugin-level | unchanged |

### Degraded / redesigned in native mode

| Feature | Impact |
|---|---|
| Call waiting / switchboard / auto-hold queue | `PhoneCall.Hold`/`ResumeFromHold` exist and *may* map to CHLD — but precise swap semantics, CLCC topology judges, parked `CallVoiceContext`, and the FIFO cascade cannot be replicated. Best case: simplified "answer the knock, hold the first caller" flow. This is the **biggest functional gap** — spike must probe what Hold actually does on a Pixel. |
| `phone.connect` / `phone.disconnect` | Connection lifecycle is Windows-owned; these become no-ops or "open Windows Bluetooth settings". Reconnect is automatic when in range. |
| `hfpCodec` (mSBC/CVSD choice) | Gone — Windows negotiates; may silently fall back to 8 kHz on some radio/phone combos. |
| Diagnostics | `dongle.diagnostics` USB/ACL/SCO counters replaced by HFP-line/endpoint status. No more corruption watchdogs, RSSI, supervision knobs (that's mostly good news). |
| Pairing UX | Windows Settings pairing (or our UI driving custom pairing). Link-key store, SSP hold window, legacy PIN: gone. |

### Dongle-mode-only (why we keep the WinUSB path)

- Full switchboard/auto-hold queue (CHLD juggling, verified swaps, parked contexts).
- Codec forcing, CIND/indicator quirks handling, raw HCI control.
- Linux support (native mode is Windows-only by definition).

## 5. Proposed architecture

```
                 aokie-plugin (radio.rs voice pipeline — unchanged)
                                │
                    RadioBackend  ← NEW trait at the current BluetoothManager seam
                   (events out / PCM in-out / ControlCommand in / status atomics)
                  ┌─────────────┴──────────────┐
        UsbRadioBackend (today)        NativeBtBackend (NEW crate: aokie-winbt)
        BluetoothManager+AokieRuntime   • WinRT Calls API (call control, caller ID)
        (WinUSB HCI host stack)         • WASAPI HF endpoints (audio pump → 16 kHz PCM)
                                        • WinRT RFCOMM → MAP/PBAP (reuses obex/map/pbap)
                                        • RfcommServiceProvider → MNS
                                        • DeviceInformation.Pairing (pairing UX)
```

Key points:

1. **`RadioBackend` trait** at the `BluetoothManager` seam (`aokie-dongle/src/bluetooth/mod.rs:140`).
   Methods mirror today's surface: `try_recv_event`, `try_recv_audio`, `send_audio`,
   `flush_tx_audio`, `answer/reject/hangup/dial/hold_swap`, `send_sms`, status getters.
   The plugin's ~13 `bt: &mut BluetoothManager` signatures become `bt: &mut dyn RadioBackend`.
   The existing concrete type becomes `UsbRadioBackend` unchanged.
2. **New crate `crates/aokie-winbt`** (Windows-only) implementing the trait on
   `windows-rs` (`Windows.ApplicationModel.Calls`, `Windows.Devices.Bluetooth.Rfcomm`,
   `Windows.Win32.Media.Audio` WASAPI). An internal tokio task owns the WinRT objects and
   feeds the same `RuntimeEvent`-shaped stream the plugin already consumes.
3. **Massive reuse:** `obex.rs`, `map_mas.rs`, `map_listing.rs`, `bmessage.rs`, `pbap.rs`,
   `vcard.rs` are byte-stream state machines — they move to the new transport behind a tiny
   `BtChannel` read/write adapter (WinRT `StreamSocket` ↔ the existing OBEX code). No
   protocol rewrites.
4. **Event parity:** `NativeBtBackend` emits the same `aokie.call.incoming/answered/ended/
   caller_id/waiting` and `aokie.sms.*` events, so the pack, flows, SMS loop, after-call
   actions and Companion all keep working unmodified.
5. **Setting `transportMode`:** `auto` (default: native if a working Windows BT radio +
   Calls-API access, else dongle if configured) | `native` | `dongle`. Dongle-mode settings
   (`hfpCodec`, `reenumerateHwid`, `legacyPairingPin`) become dongle-scoped.
6. **What shrinks over time:** in native mode the driver helper, dongle catalog, WinUSB
   transport, pairing store, corruption watchdogs and iso pacing are simply never loaded.
   They stay in-tree for dongle mode.

## 6. Phased plan

### Phase 0 — Feasibility spike (go/no-go, ~2–4 days) ⚠️ DO FIRST

A throwaway console binary (`windows-rs`), run on the live box with the Pixel:

1. **Calls API from an unpackaged process** (the go/no-go): does
   `PhoneCallStore.RequestLineWatcher()` work from a plain exe? If it throws on
   capability/identity, try: declaring the restricted capabilities via a sparse package /
   MSIX identity. **If no identity path works, native mode is dead — stop here.**
2. Register `PhoneLineTransportDevice`, pair the Pixel, verify a `PhoneLine` appears;
   incoming call → offering event with caller ID → `AcceptIncoming` → connected →
   `End`. Outbound `PhoneLine.Dial` (check the foreground requirement).
3. WASAPI: enumerate HF render+capture endpoints, stream 16 kHz both ways during a call,
   measure round-trip latency (barge-in budget) and check WBS negotiation (16 kHz vs 8 kHz).
4. RFCOMM: open MAP MAS `0x1132` while HFP is connected, OBEX connect + inbox listing;
   host MNS via `RfcommServiceProvider`, verify the phone connects on subscribe.
5. Two-call probe: second call while first is up — what does the watcher show, and does
   `PhoneCall.Hold` do something useful (CHLD-equivalent)?
6. Conflict check: Phone Link app presence/absence (it uses the same APIs + MAP).

**Go criteria:** 1–4 all pass on the Pixel. **No-go fallback:** keep dongle-only and invest
the effort in onboarding polish instead (better driver UX, restore-original-driver flow).

### Phase 1 — `RadioBackend` trait extraction (~2–3 days)

- Introduce the trait, rename the concrete `BluetoothManager` usage to `UsbRadioBackend`,
  make the plugin generic at the seam. Zero behavior change.
- All existing suites stay green (420 bluetooth + 460 plugin + synthetic-audio rig), which
  also proves the seam is complete.

### Phase 2 — `aokie-winbt` native backend (~2–4 weeks, the bulk)

- Calls-API call-control engine → `RuntimeEvent` parity (incoming/answered/ended/caller-id).
- WASAPI audio pump: capture → 16 kHz PCM channel; TX pacer → render stream; `send_audio`/
  `flush_tx_audio` semantics preserved (barge + AEC reference taps at the same points).
- RFCOMM transport adapter + wire up reused MAP/PBAP modules (SMS send/receive first,
  MNS later — PollInbox fallback keeps replies working from day one).
- Pairing flow (custom pairing) + Device Setup UX copy for native mode.
- Setting `transportMode`, plugin boot selection, health/diagnostics mapping.

### Phase 3 — Feature reconciliation + polish (~1–2 weeks)

- Call-waiting: ship whatever the spike proved (at minimum: honest `aokie.call.waiting`
  event + answer-the-knock; document switchboard as dongle-mode-only).
- Device Setup UI: hide driver-install card in native mode; pairing instructions; pack
  screen copy updates (`DonglesCard.tsx`, `PhonesCard.tsx`, manifest UI tabs).
- Docs: `docs/HARDWARE.md` native-mode section, ops notes, AGENTS.md updates.

### Phase 4 — Soak + rollout

- Hardware matrix soak: Pixel + built-in laptop BT + 1–2 common dongles on stock drivers;
  WBS vs narrowband observed; call-drop rates vs dongle mode.
- `transportMode=auto` default for new installs; existing installs stay dongle.

## 7. Risks & open questions

| Risk / unknown | Mitigation |
|---|---|
| **Calls API unavailable from unpackaged app** (restricted capabilities) | Phase-0 spike item #1; sparse-package identity is the fallback; total no-go = abandon native mode |
| Pixel + Windows HFP stability (anecdotal drops reported on Win11) | Phase-0 + Phase-4 soak on the exact live hardware |
| Phone Link contention (uses the same Calls APIs and MAP) | Spike item 6; UX note to disable Phone Link calling |
| WASAPI latency hurting barge-in/AEC feel | Spike item 3 measures round-trip; speexdsp tail/framesize tuning; worst case slightly higher `bargeSensitivity` |
| Call-waiting semantics much weaker than CHLD | Accept as native-mode limitation; switchboard stays the dongle-mode selling point |
| MAP MAS one-session-per-phone limits (if Windows/Phone Link also uses MAP) | Spike item 4/6; PollInbox cadence as fallback |
| WBS not negotiated on some radios (8 kHz audio) | Plugin already handles 8 kHz; surface codec in diagnostics |
| `PhoneLine.Dial` foreground requirement breaking headless callbacks (missed-call ring-back!) | Spike item 2 must test dialing from a background service; if blocked, outbound callbacks may be dongle-mode-only — **important for the callback-queue feature** |

## 8. Alternatives considered

- **Status quo + polish the driver install:** smaller effort, keeps every feature, but the
  dongle-hijack onboarding pain stays. Rejected as the *only* plan — it's the fallback if
  the spike fails.
- **Disable Windows HFP, do our own AT over WinRT RFCOMM:** impossible — no SCO audio path
  without Windows HFP (§3). Dead end.
- **Hybrid: dongle for AT control + Windows for audio:** one controller can't serve two
  host stacks; both would fight over the same phone. Dead end.
- **TAPI for call control:** no documented Bluetooth HFP TSP on Win10/11. Dead end.
- **LE Audio / TMAP (Windows 11 22H2+):** strategically interesting long-term (telephony
  over BLE with modern APIs), but phone-side support and API surface are immature today.
  Worth a watch item, not a plan.

## 9. Decision points for the user

1. Approve the **Phase-0 spike** (2–4 days, throwaway code, no product changes).
2. Confirm the **dual-mode** stance: native as the easy default, dongle kept for the
   switchboard/hold-queue and Linux. (Full-replacement would mean deleting the switchboard —
   not recommended given it's live-proven and recently shipped.)
3. If the spike shows `PhoneLine.Dial` needs foreground, decide whether missed-call
   auto-callbacks being dongle-mode-only is acceptable in native mode v1.

---

### Appendix A — key code references (today's architecture)

- Plugin ↔ radio seam: `crates/aokie-dongle/src/bluetooth/mod.rs:140` (`BluetoothManager`),
  `crates/aokie-bluetooth/src/aokie_radio/runtime.rs:466` (`AokieRuntime` channel API).
- WinUSB transport: `crates/aokie-bluetooth/src/aokie_radio/winusb.rs` (2,815 ln);
  cfg facade `transport.rs:1-37`; Linux twin `libusb.rs`.
- Host stack: `hci.rs`, `l2cap.rs`, `rfcomm.rs`, `sdp.rs`/`sdp_client.rs`, `hfp.rs`/
  `hfp_client.rs`/`hfp_connect.rs`, `obex.rs`, `map_mas.rs`/`map_mns.rs`/`map_runtime.rs`,
  `pbap*.rs`, `sco.rs`, `msbc/` — all under `crates/aokie-bluetooth/src/`.
- Driver layer: `crates/aokie-dongle/src/aokie_dongle/` (`winusb.rs`, `installer.rs`,
  `pki.rs`) + `src/bin/aokie-driver-helper.rs`.
- Voice pipeline (transport-agnostic): `crates/aokie-plugin/src/radio.rs`; audio seam
  `AudioLink`/`FakeLink` at `radio.rs:3250`; synthetic-audio rig `radio.rs:17492`.
- Switchboard/call-waiting: `radio.rs:5172-5430` (swap judges), `:7231-7400` (reconciliation/
  cascade), `holdAndCallWaiting`/`autoHoldQueue` settings in `connector.rs:3530+`.
- Reusable protocol modules for native mode: `obex.rs`, `map_mas.rs`, `map_listing.rs`,
  `bmessage.rs`, `pbap.rs`, `vcard.rs` (byte-stream machines, transport-independent).

### Appendix B — verified Windows API citations

- No user-mode SCO: <https://learn.microsoft.com/en-us/windows/win32/bluetooth/bluetooth-and-socket>,
  <https://learn.microsoft.com/en-us/windows-hardware/drivers/bluetooth/bluetooth-profile-drivers-overview>
- HFP audio endpoints + WBS + on-demand SCO: <https://learn.microsoft.com/en-us/windows-hardware/drivers/bluetooth/bluetooth-classic-audio>
- Windows BT version support (Win11 22H2 = HFP 1.7.2): <https://learn.microsoft.com/en-us/windows-hardware/drivers/bluetooth/general-bluetooth-support-in-windows>
- RFCOMM client/hosting: <https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.rfcomm.rfcommdeviceservice>,
  <https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.rfcomm.rfcommserviceprovider>
- Calls APIs: <https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.calls.phonelinetransportdevice>,
  <https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.calls.phoneline>,
  <https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.calls.phonecall>
- Capability rules: <https://learn.microsoft.com/en-us/windows/uwp/packaging/app-capability-declarations>
- Pairing: <https://learn.microsoft.com/en-us/windows/apps/develop/devices-sensors/pair-devices>
