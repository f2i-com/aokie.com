# Aokie Companion iOS native runtime (build-gated)

This Swift package is source-level platform code for a future generated Tauri
iOS project. It is intentionally not described as production-ready:

- it has not been compiled with Xcode or run on an iPhone from this Windows
  workspace;
- the app target does not yet link/register the package through a Tauri plugin
  or native Rust FFI adapter;
- Push Notifications, Voice over IP background mode, APNs environment, and
  signing/provisioning entitlements are not present;
- the FormLogic registration route exists, but this build-gated package is not
  yet connected to it through a native Rust/Swift adapter and no APNs provider
  has been configured to send the strict schema-1 PushKit payload;
- exact protocol-v2 winner/cancellation events are not yet wired to
  `reconcileWinner` / `reconcileCancellation`;
- CallKit, process-death, locked-device, notification, permission-revocation,
  route/interruption, direct WebRTC, and TURN behavior require physical-device
  tests.

The package nevertheless fixes the intended native boundary in source:

- OAuth restart material is Keychain-only with
  `AfterFirstUnlockThisDeviceOnly`; there is no WebView fallback.
- PushKit validates and persists an opaque short-lived offer before UI code and
  reports a real CallKit call.
- CallKit Answer requests authority but cannot arm the microphone. Exact
  call/epoch winner reconciliation gates audio.
- AVAudioSession exposes media only after CallKit activation, exact authority,
  and microphone permission. Permission loss/interruption synchronously asks
  the native media adapter to disarm and close.
- Standard APNs and VoIP tokens remain native, use separate ThisDeviceOnly
  Keychain records, include their exact bundle topics in native enrollment
  callbacks, and are cleared together during native logout.

When a macOS/Xcode build environment is available, first generate the Tauri
iOS project, link this package into the app target, implement the two native
sink protocols in a non-WebView Tauri plugin, add the required entitlements,
then run `swift test` and physical-device tests before changing this status.

`Integration/` contains illustrative app-target fragments only. Enable Xcode's
Push Notifications capability and the Audio/Voice-over-IP background modes,
and let the selected provisioning profile supply the correct APNs environment.
Do not copy a development APNs entitlement into a production archive.
