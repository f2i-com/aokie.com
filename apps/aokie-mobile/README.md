# Aokie Companion

A native Tauri v2 microphone/speaker endpoint for Aokie calls. Companion can
run as a Windows app or a mobile app. It can read permission-filtered call
state and captions, listen without transmitting, take over a caller, and
return the caller to Aokie.

Companion never connects to the Bluetooth dongle. The call path is:

```text
cellular phone <-> Bluetooth dongle <-> Aokie Desktop
                                      <==== WebRTC media ====> Companion
                                      (direct or encrypted TURN relay)

          FormLogic or custom server: auth, signalling and routing only
                                      (never call PCM)
```

Running Companion on the same Windows PC as Aokie Desktop is supported. It is
not blanket-disabled on any phone either. If an Android phone is itself the
carrier/HFP gateway for the cellular call, simultaneous HFP and WebRTC audio
may contend for that device's microphone and speaker; that one topology needs
physical-device route testing. Using a local signalling server does not change
that audio-route constraint because signalling servers do not carry the audio.

## Implemented

- Production Vite/React/TypeScript UI with bundled assets and a strict Tauri
  CSP.
- Native protocol-v2 realtime client with a mandatory authoritative snapshot,
  bounded frames and queues, ping/pong liveness, reconnect, lease heartbeats,
  and exact app/device/call/epoch/revision binding.
- Native WebRTC media using `../../crates/aokie-media`. Monitor and prepared
  consult/takeover are receive-only. Active private consult arms this endpoint's
  microphone only after Desktop proves caller hold; active takeover requires
  the current rotated native lease and remote-audio evidence.
- A native local-media proof. The UI says `YOU ARE LIVE` only when the exact
  current takeover session has connected caller audio and an armed microphone.
  Private consult separately proves that the caller is held and the microphone
  is routed only to Aokie, never to caller SCO.
- Protocol-v2 listen, private-consult, takeover, revoke, return, SDP, and ICE wiring.
  Lease tokens and gateway admissions remain in Rust and never enter the
  WebView.
- Strict signed schema-v2 discovery. The native verifier rejects redirects,
  unknown fields, cross-origin OAuth/admission/key URLs, invalid media topology,
  and signature-envelope mismatches, and verifies Ed25519 signatures over the
  canonical payload.
- Managed OAuth using the system browser, authorization code + PKCE S256, an
  RFC 8252 loopback callback, app/device binding, rotating refresh tokens in
  native secure storage, startup restore, and a fresh short-lived admission
  before every WebSocket connection attempt. Windows uses Credential Manager;
  Android uses a non-exportable Android Keystore AES-256-GCM key.
- Typed native account APIs for bootstrap, redacted activity/session history,
  team routing, and availability. Rust derives fixed routes from the verified
  OAuth-resource origin, owns refresh/401 retry, rejects redirects and unknown
  response fields, and exposes no bearer to React.
- Explicit custom-server connection for compatible self-hosted WSS gateways.
  Debug builds also allow an explicitly configured loopback `ws://` gateway.
- Legacy protocol-v1 state/control support for older local gateways.
- An explicitly labelled interactive demo that cannot contact a caller or
  request microphone permission.
- Windows Tauri packaging and generated Android project/assets. Android media
  uses `libwebrtc`, requests microphone permission only for an exact active
  consult or talk lease, polls for mid-call permission revocation, and closes
  the local peer immediately if access is revoked.
- Optional Android FCM hooks, encrypted pre-WebView offer storage, Android
  `CallStyle` notifications, and Core-Telecom call lifecycle handling. An
  answer requests the authoritative lease; it does not open the microphone.
- Native-only managed push endpoint registration/rotation/logout invalidation.
  Provider tokens remain in Keystore/Keychain; only redacted fingerprints can
  appear in account data returned to React.
- Windows microphone/speaker enumeration and selection by libwebrtc endpoint
  GUID. Selection is rejected while any media peer is active; mobile routing
  is explicitly reported as operating-system managed.

## Remaining platform and deployment gates

- Android FCM builds without Firebase credentials and reports
  `configuration_required`. Production background ringing additionally needs
  a project-specific `google-services.json`; a custom server must implement the
  documented authenticated endpoint registration and emit the data-only offer,
  cancellation, and informational messages. FormLogic implements the managed
  registration route. Push is only a wake-up hint; the
  signed protocol-v2 snapshot/lease remains authoritative.
- Android background, Doze, force-stop, notification-denial, Core-Telecom, and
  competing HFP/WebRTC audio routes still need physical-device testing.
- Source-level iOS Keychain, PushKit, CallKit and AVAudioSession integration is
  build-gated under `src-tauri/native/ios/AokieCompanionRuntime`. There is no
  generated iOS app target, Tauri native-plugin binding, Apple signing/APNs
  environment, Xcode compile, or device verification in this Windows
  workspace, so the current app must still fail closed rather than claim iOS
  native secure storage or background call support.
- A managed deployment must publish an available, correctly signed schema-v2
  discovery document and implement OAuth, device approval, admission, and the
  protocol-v2 gateway. If discovery reports `available: false`, the setup UI
  verifies the signature but correctly keeps managed sign-in locked.
- Real caller audio, Bluetooth routing, reconnect recovery, and Secure Boot
  behavior still require the target phone, dongle, and Desktop hardware path.

## Run locally

Prerequisites: Node.js 22+, Rust, and the Tauri v2 Windows prerequisites.

```powershell
npm install
npm run check
npm test
npm run build
npm run tauri dev
```

In a development build, signed discovery defaults to:

```text
http://api.formlogic.local/.well-known/aokie-companion
```

Debug builds can use the exact `formlogic.local` / `api.formlogic.local`
development origins. Normal release discovery, OAuth, admission, and signing-key
requests require native TLS. The deliberate local-pilot package may also be built
with `managed-beta-local`; that feature permits only the exact
`api.formlogic.local` API origin and a numeric loopback gateway, and must not be
used as a public-production transport policy. The API authorization endpoint
redirects the system browser to the trusted `formlogic.local` consent SPA.

After successful signed discovery, choose **Sign in and connect**. The native
app re-fetches and re-verifies discovery before opening the system browser.
The browser receives only the OAuth request; tokens are exchanged and stored
by Rust.

For a custom local gateway, expand **local gateway test**. The default is
`ws://127.0.0.1:18787/v2/realtime`, `app_local`, and `device_local`. Supply a
short-lived token, or launch the debug process with
`AOKIE_COMPANION_LOCAL_TOKEN`. Release builds require `wss://`.

## Android

With Android Studio, SDK, NDK, and a device installed:

```powershell
npm run tauri android dev
```

The generated project currently requires Android Studio's JBR 21 (or another
supported JDK 21); JDK 25 is too new for its Gradle toolchain. On Windows,
Tauri creates native-library symlinks while packaging Android. Enable Windows
Developer Mode (or grant equivalent symlink privilege) for the standard build.

Firebase is optional at compile time. To enable production FCM, place the
project-specific file at
`src-tauri/gen/android/app/google-services.json`. It is gitignored and must not
be committed. Without it, the app remains foreground-capable and diagnostics
truthfully report `configuration_required`.

Test notification denial/revocation, background/Doze, FCM token rotation,
offer expiry/cancellation, first-winner reconciliation, audio focus, and media
on physical hardware. An emulator is not a meaningful Bluetooth/cellular
audio-route test.

## Tests

```powershell
npm run check
npm test
npm run build
cargo test -p aokie-mobile --lib
```

| Path | Responsibility |
|---|---|
| `src/bridge/` | Strict native bridge and explicit demo/unavailable adapters |
| `src/state/` | Monotonic legacy snapshot and UI state invariants |
| `src-tauri/src/discovery.rs` | Signed native discovery boundary |
| `src-tauri/src/managed_auth.rs` | Native OAuth, secure refresh, and admission |
| `src-tauri/src/realtime.rs` | Legacy realtime plus protocol selection |
| `src-tauri/src/realtime_v2.rs` | Native v2 snapshot, lease, and RTC adapter |
| `src-tauri/src/media.rs` | Native peer lifecycle and truthful media proof |
| `../../crates/aokie-media/` | Platform-independent WebRTC media primitives |
| `../../crates/aokie-protocol/` | Canonical protocol models and invariants |

## Security invariants

- No tenant-controlled HTML is loaded into the privileged WebView.
- OAuth, refresh, admission, and lease tokens remain native-only.
- Companion account reads/writes use the active native managed session; the
  renderer can request typed records but cannot supply or receive a bearer.
- Managed admission is reacquired before each WebSocket reconnect.
- Monitoring creates no microphone track. Prepared consult/takeover cannot arm
  the microphone. Active consult can route microphone audio only to Aokie's
  isolated WebRTC lane, and active takeover can route it only after the rotated
  caller-ownership lease; neither path can be promoted by UI state alone.
- Any transport, lease, epoch, revision, or session mismatch closes/replaces
  the native peer and clears local media proof.
- Offline or reconnecting state locks mutations; queued commands are scoped to
  one authenticated socket generation.
- Browser preview never silently substitutes demo data. Demo state is visibly
  labelled and has no live media authority.
