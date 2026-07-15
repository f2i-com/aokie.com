# Aokie — Supported Hardware

Aokie drives the Bluetooth radio directly over WinUSB, so it is bound to
specific dongle chipsets rather than to whatever Windows' own Bluetooth stack
supports. The authoritative list is `crates/aokie-core/src/dongle_catalog.rs`
(`DEFAULT_CATALOG`) — this document is its human-readable companion.

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

## Adding a dongle

First exercise an external controller through managed-beta unknown-device mode.
If endpoint probing, pairing, HFP, bidirectional SCO, MAP and restoration pass,
add a `DongleId` to `DEFAULT_CATALOG` with a conservative Beta tier, rebuild, and
run the full lifecycle against it (ring → answer → hear → speak → SMS). Until
the SCO path is confirmed to carry audio both ways on that chipset, keep it at
Beta.
