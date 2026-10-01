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
- `transcribe-rs`'s `openai` feature is built, linted and tested with the workspace
  (`--features transcribe-rs/openai`). Its tests talk to a loopback server; the one test that calls
  the real OpenAI API is ignored unless asked for.
- Both `ort` declarations must disable default features and select `api-25`
  while the app ships ONNX Runtime 1.25. The defaults in rc.13 select a newer
  API, which would reject the bundled DLL at runtime. Preserve dynamic loading
  so Sherpa's separate ONNX Runtime 1.17 library cannot be selected accidentally.
- The self-hosted gateway has a separate lockfile in
  `deploy/companion-self-host/container-workspace`. Refresh and test that
  two-crate workspace when its dependencies change.

The GLib 0.18 GTK compatibility backport and its Linux release-mode regression
test are documented in [vendor/README.md](../vendor/README.md). Do not replace
it with an additional newer GLib dependency: the old GTK dependency would remain.

On recent Windows installations, Sherpa 0.6.8's source build can fail because
its CMake OS-description probe invokes the removed `wmic` executable. Its
supported `SHERPA_LIB_PATH` setting can use an existing matching Sherpa build:
point it at the installation directory containing `lib/` and `include/`.
The September 2026 native checks used this path with the existing 0.6.8 binaries.
Do not use `SHERPA_SKIP_GENERATE_BINDINGS`; that published crate lacks the
pre-generated bindings it expects.
