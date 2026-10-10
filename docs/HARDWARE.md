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

**A bundle with nothing to build.** `scripts/bundle-unix.sh` lays the plugin
out as a folder, and the workflow **Unix bundles** (macOS, below, says how it is
started) makes that folder on Ubuntu 22.04 as
`aokie-plugin-linux-x86_64.tar.gz`. With the workflow's own commands in an
Ubuntu 22.04 container, that archive was built and then installed into OAIY
Desktop (its `.deb`) on a clean Ubuntu 24.04 by its path, as the `.tar.gz` and
as a `.zip` of the same folder, trusted, and started: the plugin answers OAIY,
and with no consent recorded and no dongle it keeps its radio off and says so.
It uses the system's libusb (`libusb-1.0-0`), which a desktop install has; where
that is missing the plugin does not start and its log names the library.

## macOS

**Built on a Mac, and never run on one with a dongle.** The plugin with voice
has been built on GitHub's macOS 15 Apple-silicon runner (the workflow **Unix
bundles**, 10 October 2026), and its bundle started there from its folder: it
answers OAIY's first calls, finds no dongle, keeps its radio off and shuts down
cleanly. Two things only a Mac build could show were met and fixed on the way:
libwebrtc's Objective-C categories have to be linked in (`-ObjC`, in
`.cargo/config.toml`), or the plugin aborts while it is loaded; and the build's
libclang has to be named (`scripts/bundle-unix.sh` does). Nobody has had it on
a Mac with a dongle, a phone or a Keychain prompt: the radio, pairing, calls
and texts there are untried.

Everything else below was made and checked without a Mac. The Mac shares its
code with Linux: the phone link is compiled
wherever the radio stack has a USB transport (Windows, Linux, macOS), and only
what calls Windows itself stays Windows'. So the same code was built, tested
and run on Linux, against a stand-in dongle (`scripts/virtual-dongle`). For an
Apple-silicon Mac it was type-checked from a Windows PC as far as that goes:
the radio stack, the dongle bridge, the sealing, and the plugin without its
speech stack (nothing was linked). The plugin with voice could not be checked
for macOS from Windows, because its speech libraries' build scripts need the
Mac's own tools; that part rests on the Linux build, which shares its code.
What only a Mac can answer is listed at the end.

### What a Mac build is

* **The same plugin.** Calls, texts, contacts, pairing, the receptionist: the
  radio stack and everything above it are the code Windows runs.
* **No driver.** Aokie opens the dongle through libusb, which is built into
  the program (no Homebrew needed). Nothing is installed and nothing has to be
  restored afterwards. `dongle.list` says so (`driverModel: "none"`), and the
  dongle setup screen then has no driver steps: it lists the dongle and, when
  the radio has not opened it, says why in the radio's own words.
* **No Bluetooth permission.** Aokie does not use macOS's Bluetooth at all: it
  talks to the dongle over raw USB.
* **Secrets in the Keychain.** Where Windows seals with DPAPI (paired phones'
  link keys, the manager PIN, the outbox's call and text records), a Mac seals
  with AES-256-GCM under a key kept in your login Keychain. The rule is the
  same: nothing is ever stored unsealed, and a Keychain that will not give the
  key stops the write. macOS ties "Always Allow" to a program's signature, so
  a plugin you have rebuilt is a new program to the Keychain, which asks again.
* **`transportMode`** is `dongle`. `native` is Windows' own Bluetooth stack and
  is refused with a message that says so.

### The open question: who has the dongle

Whether macOS's own Bluetooth takes the dongle when it is plugged in differs
between Macs and macOS versions.

* **macOS leaves it alone:** Aokie claims the dongle as an ordinary user.
* **macOS's Bluetooth holds it:** libusb may take a device from a macOS driver
  only for a process that runs as root, or one that carries the
  `com.apple.vm.device-access` entitlement, which Apple grants to
  virtualisation apps. Aokie then says so and does not start. Do **not** run
  the app that hosts Aokie with `sudo`: its Keychain would be root's, and the
  files it writes under your home folder would become root's.

To see which it is, with the dongle plugged in:

```sh
cargo run -p aokie-bluetooth --example dongle_probe
```

It lists each USB Bluetooth controller with its endpoints (from descriptors:
nothing is touched), then opens one and asks it who it is (an HCI Reset: the
dongle is not paired or changed). `opened, reset and answering` means Aokie can
use it as you. If it says macOS's Bluetooth is using the dongle, running that
one probe again under `sudo` tells whether taking it from macOS works at all on
this Mac; that is a test, not a way to run Aokie. OAIY's `tools/mac/doctor.sh`
prints the same picture from macOS's side (its "USB Bluetooth dongle" section).

**Which controller.** On Windows only a dongle bound to Aokie's driver can be
opened. Here no driver marks a dongle, and every USB Bluetooth controller is a
candidate: on an Intel Mac or a Linux laptop the computer's own Bluetooth is
one of them. The probe opens the only controller it finds, or the one whose
path you give it, never several. Aokie's radio opens the first it finds unless
one has been chosen; with more than one connected, the dongle screen offers
**Use this dongle** (`dongle.setPreferred`), and a chosen dongle that is not
plugged in is not replaced by another controller. An Apple-silicon Mac's own
Bluetooth is not a USB device, so there the dongle is the only one.

### Building and trying it

Xcode's command-line tools, Rust and `cmake` are needed (the speech stack has
native parts). Then:

```sh
sh scripts/bundle-unix.sh
```

builds the plugin with voice and lays out the folder OAIY Desktop loads, with
the manifest's entry changed to `aokie-plugin` (the one difference from the
Windows manifest) and ONNX Runtime 1.25.0 fetched for the speech engines that
run on it (Microsoft publishes it for Apple silicon, not for Intel Macs, where
those engines stay off). Its last lines say where to copy the folder; OAIY Desktop
then asks you to trust the plugin, as it does for any plugin that is not
signed. `AOKIE_FEATURES="" sh scripts/bundle-unix.sh` builds without the speech
stack: a plugin that pairs, texts and lets calls ring through, and a much
shorter build to find out whether the dongle works at all.

**Without building on the Mac.** GitHub's Mac can make the same folder: on the
repository's page, **Actions**, **Unix bundles**, **Run workflow**
(`.github/workflows/unix-bundle.yml`; it runs only when started like that, and
publishes nothing). Each of its legs, one on a Mac and one on Ubuntu, runs
`bundle-unix.sh` and uploads `aokie-plugin-macos-arm64.tar.gz` or
`aokie-plugin-linux-x86_64.tar.gz` as the run's artifact. In OAIY Desktop, under
**Connections**, **Plugins**, give that file's path to install it, then **Trust
this plugin** and **Start**. Nothing has to be installed on the Mac that uses
it: no Rust, no cmake, no Xcode tools. Before the upload each bundle is started
from its folder on the build machine and asked what OAIY asks a plugin first
(`python3 scripts/virtual-dongle/plugin_check.py --no-dongle <the program>`): it
answers, lists no dongle, keeps its radio off, and shuts down cleanly. That says
the bundle runs from its folder on that system, and nothing about a dongle.
The workflow uses no build cache: a build folder that comes back from one has
lost the speech libraries a build script had put in it, and `bundle-unix.sh`
now builds that part again when it finds them missing rather than lay out a
plugin that cannot start.
The Mac's programs are signed there by nobody (ad hoc), which is what an
Apple-silicon Mac asks before it runs a program; a downloaded one may still be
held back by macOS until you allow it, as OAIY's own app is (OAIY's
`docs/MAC.md`).

A plugin built with voice fetches its speech models the first time it starts,
before its radio thread reports the dongle (as on Windows). That takes minutes
on a first start: until it is done the dongle screen says the radio is not up,
and its "Start Aokie again" gives up after half a minute. Wait for the
download rather than starting again; the plugin's log says what it is fetching.

The tests: `AOKIE_SEAL_EPHEMERAL=1 cargo test --workspace` (and the same in
front of `cargo test -p aokie-plugin --features voice`). Several crates' tests
seal something (a paired phone's link key, the manager PIN); the variable makes
a debug build seal under a key that lives in the test process only, so a test
run neither writes to your Keychain nor waits on its prompt, once for every
test program. Release builds ignore it. No test covers the Keychain itself, or
the outbox sealed with it: a test's outbox is not sealed off Windows.

A build for other people is another matter: it would have to be signed and
notarised, and libusb is LGPL-2.1, which sets terms for a library linked into a
program (`crates/aokie-bluetooth/Cargo.toml` says what the choices are).

### What only a Mac can answer

1. Who has the dongle (above).
2. libusb's macOS half. The transport's own code ran on Linux; libusb's IOKit
   backend under it did not. Events and data are plain transfers; call audio
   rides isochronous transfers, whose timing through libusb on macOS has not
   been measured. Expect texts and pairing to be the first things that work
   and call audio to be the part that needs a Mac to tune.
3. Whether the speech stack's native libraries build there (sherpa-onnx,
   speexdsp, libwebrtc) and are found beside the plugin when it starts.
4. The Keychain's prompts for a plugin that OAIY Desktop starts.
5. OAIY Desktop loading a plugin on macOS at all.

## Adding a dongle

First exercise an external controller through managed-beta unknown-device mode.
If endpoint probing, pairing, HFP, bidirectional SCO, MAP and restoration pass,
add a `DongleId` to `DEFAULT_CATALOG` with a conservative Beta tier, rebuild, and
run the full lifecycle against it (ring → answer → hear → speak → SMS). Until
the SCO path is confirmed to carry audio both ways on that chipset, keep it at
Beta.
