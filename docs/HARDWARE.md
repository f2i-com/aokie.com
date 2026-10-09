# Supported hardware

**The WinUSB dongle is the only supported phone-link transport for calls.**
Aokie's userspace Bluetooth host stack drives a supported USB dongle directly
over WinUSB — that is the only configuration that carries call audio. The
authoritative dongle list is `crates/aokie-core/src/dongle_catalog.rs`
(`DEFAULT_CATALOG`) — this document is its human-readable companion.

Why (verified 2026-07-20, see `docs/NATIVE_BLUETOOTH_TRANSPORT_PLAN.md` for the
full findings): Windows 11 25H2 no longer ships the Bluetooth hands-free-unit
service (`BthHFSrv.dll`) that exposed call audio to applications, SCO/eSCO
audio is kernel-mode-only, LE Audio telephony does not engage on the evaluated
hardware, and Phone Link's call audio is a proprietary network channel — so
there is no user-mode path to Bluetooth call audio on current Windows.

A **native Windows-Bluetooth backend** (`crates/aokie-winbt`,
`transportMode=native`) exists for advanced use: with stock Windows drivers it
provides SMS (MAP), contacts (PBAP), pairing, and call *control* (ring/answer/
end events) on Windows builds where the hands-free service exists. It is not
offered in the settings UI because it cannot carry call audio on Windows 11
25H2 — set it only deliberately via `settings.set`.

## Dongle compatibility tiers

Anything not in the catalog is treated as **Unverified**. Standard production
builds do not bind it. A managed-beta build can admit an unlisted external USB
Bluetooth HCI controller with the deliberate
`AOKIE_INSTALL_UNKNOWN_DONGLE=YES_I_REBIND_AT_MY_OWN_RISK` operator opt-in.
That is a probe path, not a compatibility promise. Tiers reflect how much of
the HFP/SCO/MAP path has been exercised on real silicon.

| Chipset | VID:PID | Tier | Notes |
|---|---|---|---|
| Broadcom BCM20702 | `0a5c:21e8` | Certified | Common 2.4 GHz combo dongle |
| Broadcom BCM20702 | `0a5c:21ec` | Certified | Variant of the above |
| Realtek RTL8761 | `0bda:8771` | Beta | Chipset in many ASUS / TP-Link dongles |
| Realtek RTL8821CE | `0bda:c822` | Beta | Combo cards in some HP / Lenovo laptops |
| CSR CSR8510 A10 | `0a12:0001` | Beta | Ubiquitous BT 4.0 dongle; no vendor firmware load |

**Certified** = the full answer/hear/speak/SMS path has been run end to end.
**Beta** = enumerates and pairs; the SCO audio path may vary by firmware.

## Phones

HFP (call control + audio), MAP (SMS) and PBAP (contacts) behaviour varies by
handset OS and firmware. Aokie has been exercised against mainstream Android
and iOS builds; there is no certified-phone matrix yet — establishing one is
the hardware half of audit task **AOK-E2E-001** (a record/replay radio
abstraction plus a small supported-device lab).

### Android 17: texts held by the Google Messages beta

`aokie.sms.sent` means the phone accepted the MAP PushMessage into its outbox
(OBEX `0xA0`), not that the text went out. The phone reports the real result
only over MNS, which outbound sessions never get. Since 28 Sept 2026 the radio
logs a **sent check** 20 s after each send: the phone's outbox and sent folder
listings (`[AokieRadio] sent check: Outbox holds …`).

What the phone does with a pushed text, read from its logs and its own code on
the Pixel 9a test phone (Android 17, CP3A.260905.009) on 9 Oct 2026:

- Its Bluetooth message service stores the text and asks the phone service to
  send the stored one (`SmsController: sendStoredText caller=com.google.android.bluetooth`
  in `adb logcat -b radio`).
- Android 17 hands every text from an app that is not the default SMS app to
  the default app first, when that app offers a service for
  `android.service.messaging.AlternativeMessageTransportService`
  (`SMSDispatcher: sendText: requesting message upgrade via DMA.`). Only the
  sender's package is looked at. **A sender cannot opt out**: nothing in a MAP
  push changes it, the phone turns a text-only MMS push back into an SMS, and
  the switch for it is fixed on in Android 17.
- Google Messages offers that service (its "RCS upgrade"), and what happens
  next is the build's:
  - The factory build (`messages.android_20260331_00_RC04`) refuses the
    hand-over (`sendText: message upgrade request failed.`) and the phone sends
    the text itself within a second.
  - The open beta (`20260921_01_RC00`, and `20261002_00_RC00` still) accepts
    it. With RCS chats off it makes an SMS copy of its own and queues it. The
    stored original stays in the conversation as a second message on "Still
    sending", and the queue stalls on it: the copy went out only when the next
    message was sent (15 s late once, not at all in 4½ minutes another time).
    A text typed in Messages in the same conversation then showed "Not sent"
    although the network had taken it.

The fix is on the phone: `adb shell pm uninstall-system-updates
com.google.android.apps.messaging` puts the factory build back (Messages' own
data starts over; the texts are read again from the phone's store), and
leaving the Messages beta in the Play Store keeps it there (still enrolled,
Play put the beta back on 8 Oct). Checked again on 9 Oct 2026: the two texts
pushed after the rollback were refused the hand-over and sent at once. It was
not a Bluetooth, SIM or carrier fault, and turning RCS chats off in the beta
does not help.

On the beta the sent check cannot tell a sent text from a held one: the
stored original stays in the outbox either way.

## Windows

Windows 10/11 x64. The plugin raises the process timer resolution to 1 ms for
the radio's lifetime — the SCO isochronous path services USB frames every
millisecond and the default ~15.6 ms tick starves it.

WinUSB replaces the selected dongle's normal Windows Bluetooth binding while
Aokie owns it. Aokie therefore targets explicit external USB dongles only and
refuses composite/internal adapters. Discovery and HCI/ACL may work on many
standards-compliant controllers, while bidirectional SCO audio is the most
likely vendor-specific failure point.

## Linux

Linux uses libusb rather than WinUSB. The selected VID/PID needs an appropriate
udev permission rule, and Aokie may need to detach the kernel `btusb` driver for
the session. The shared radio stack is portable, but Linux packaging and the
real-hardware call/SMS/audio matrix are not yet release-qualified.

The libusb transport has not talked to a real dongle yet. What it has been run
against is a stand-in: `scripts/virtual-dongle` serves a USB Bluetooth
controller over USB/IP, the kernel attaches it and binds `btusb` to it as it
would a real one, and a test drives the transport through taking the device
from `btusb`, bring-up, events, ACL data, a voice link on each codec's
alternate setting, and the dongle being pulled out. That proves the USB
plumbing, not call audio: see the README there for what it can and cannot say.

## macOS

**Not yet run on a Mac.** The radio stack's USB transport builds for macOS
(Apple silicon and Intel) and is the same libusb transport Linux uses; what
follows is what the code does and what only a Mac can answer.

No driver is installed and nothing has to be restored afterwards: Aokie opens
the dongle through libusb, which is built into the program (no Homebrew
needed). Bluetooth permission is not asked for, because Aokie does not use
macOS's Bluetooth at all: it talks to the dongle over raw USB.

The open question is whether macOS's own Bluetooth takes the dongle when it is
plugged in. Some Macs and macOS versions attach their Bluetooth to an external
USB controller and some leave it alone.

* **macOS leaves it alone:** Aokie claims the dongle as an ordinary user.
* **macOS's Bluetooth holds it:** libusb may take a device from a macOS driver
  only for a process that runs as root, or one that carries the
  `com.apple.vm.device-access` entitlement, which Apple grants to
  virtualisation apps. Aokie then says so and does not start. Do **not** run
  the app that hosts Aokie with `sudo`: its keychain would be root's, and the
  files it writes under your home folder would become root's.

To see which it is, with the dongle plugged in:

```sh
cargo run -p aokie-bluetooth --example dongle_probe
```

It lists each USB Bluetooth controller with its endpoints, then opens it and
asks the controller who it is (an HCI Reset: the dongle is not paired or
changed). `OK` means Aokie can use it as you. If it says macOS's Bluetooth is
using the dongle, running that one probe again under `sudo` tells whether
taking it from macOS works at all on this Mac; that is a test, not a way to
run Aokie. OAIY's `tools/mac/doctor.sh` prints the same picture from macOS's
side (its "USB Bluetooth dongle" section).

Call audio is the part most likely to need work on real hardware: SCO audio
rides isochronous USB transfers, whose timing through libusb on macOS has not
been measured.

## Adding a dongle

First exercise an external controller through managed-beta unknown-device mode.
If endpoint probing, pairing, HFP, bidirectional SCO, MAP and restoration pass,
add a `DongleId` to `DEFAULT_CATALOG` with a conservative Beta tier, rebuild, and
run the full lifecycle against it (ring → answer → hear → speak → SMS). Until
the SCO path is confirmed to carry audio both ways on that chipset, keep it at
Beta.
