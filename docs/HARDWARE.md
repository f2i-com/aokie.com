# Aokie — Supported Hardware

Aokie drives the Bluetooth radio directly over WinUSB, so it is bound to
specific dongle chipsets rather than to whatever Windows' own Bluetooth stack
supports. The authoritative list is `crates/aokie-core/src/dongle_catalog.rs`
(`DEFAULT_CATALOG`) — this document is its human-readable companion.

## Dongle compatibility tiers

Anything not in the catalog is treated as **Unsupported** (the driver won't be
bound). Tiers reflect how much of the HFP/SCO/MAP path has been exercised on
real silicon, not a promise.

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

## Adding a dongle

Add a `DongleId` to `DEFAULT_CATALOG` with a conservative tier, rebuild, and
run the full lifecycle against it (ring → answer → hear → speak → SMS). Until
the SCO path is confirmed to carry audio both ways on that chipset, keep it at
Beta.
