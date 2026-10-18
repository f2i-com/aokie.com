# Dependency maintenance

Automatic CI is paused. Run the manual release checks before publishing updates:

```powershell
./scripts/check-release.ps1 -Companion -Audit
```

Keep these dependency groups aligned:

- Upgrade `candle-core`, `candle-nn`, and `candle-transformers` together. Their
  tensors and model APIs must use the same release.
- Keep both `libwebrtc` pins aligned: `aokie-media` and the Companion Android
  target. Build Windows with the static CRT settings in `.cargo/config.toml`.
  The Android Gradle build copies libwebrtc's Java runtime (`libwebrtc.jar`) into
  `gen/android/app/libs` from the prebuilt libwebrtc that `webrtc-sys-build` downloads
  (`gen/android/buildSrc/.../BuildTask.kt` looks for it in the Cargo target directory). After a
  `webrtc-sys` bump, check that the Android build still finds it.
- Keep the `tauri` crate and the `@tauri-apps/api` and `@tauri-apps/cli` packages in
  `apps/aokie-mobile/package.json` on the same major and minor release: `tauri dev` and
  `tauri build` refuse to run when they differ. Upgrade them in one change, together with
  `tauri-build` and `tauri-plugin-opener`, and re-check `apps/aokie-mobile/src-tauri/gen/android`
  (the activity glue, R8 rules and Gradle versions Tauri's templates move on).
- The Android build downloads `org.rustls:rustls-platform-verifier` from the GitHub-hosted Maven
  archive that `rustls-platform-verifier-android` names (`gen/android/app/build.gradle.kts`); its
  version is the one in `Cargo.lock`. After a bump of that crate, check that the archive has the
  version, that `org.rustls.platformverifier.CertificateVerifier` is still in it, and that its manifest
  still brings the network security config the debug override in
  `gen/android/app/src/debug/res/xml/` stands in for.
- The self-hosted gateway has a separate lockfile in
  `deploy/companion-self-host/container-workspace`. Refresh and test that
  two-crate workspace when its dependencies change.

The GLib 0.18 GTK compatibility backport and its Linux release-mode regression
test are documented in [vendor/README.md](../vendor/README.md). Do not replace
it with an additional newer GLib dependency: the old GTK dependency would remain.

`wry` 0.57.0 is patched the same way (`vendor/wry-0.57.0-patched`, a path entry under `[patch.crates-io]` in the
root `Cargo.toml`; `Cargo.lock` therefore has no registry `source`/`checksum` for it). On Android a late answer on
the main-thread message pump aborted the whole app: `GetWebViewVersion` is asked for while the runtime is created,
waits 10 seconds, and a slow start-up (a main thread still in `Activity.onCreate`, as on a small emulator right
after an install) first made wry panic on the dropped receiver and then made Tauri refuse to create the webview.
After a `tauri` or `wry` bump, check whether the new wry still needs the patch (the `unwrap` on `tx.send` in
`src/android/main_pipe.rs`, the 10 s wait in `platform_webview_version`), drop it if not, and otherwise re-apply the
edits marked `PATCH (aokie)` to the new source and update the checksum in `vendor/README.md`. Re-run the
12-second main-thread stall check described in `apps/aokie-mobile/README.md`.

Do not make `Activity.finish()` the Android Back behaviour again (see "Back button" in
`apps/aokie-mobile/README.md`): the destroy path ends in `process::exit`, which races Android's `libhwui` render
thread and aborts the process. `src-tauri/src/process_exit.rs` exists for the same reason; a newer tao or Tauri that
no longer calls `process::exit` on the Android destroy path would make it unnecessary.

Aokie has no speech of its own any more (OAIY Voice hears and speaks for
calls): ONNX Runtime (`ort`), sherpa-onnx, Candle, `transcribe-rs`, the voice
server and `aokie-ai` left the workspace in October 2026, and with them the
upgrade rules that tied `ort` to the bundled ONNX Runtime and the Windows
`wmic` repair for sherpa-onnx's CMake build. The echo canceller (`aec-rs`,
speexdsp) still runs bindgen, so a build needs libclang.
