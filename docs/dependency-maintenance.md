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
