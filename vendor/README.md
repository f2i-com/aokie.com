# Vendored crates

- [`glib/`](#glib-compatibility-backport): a security backport for the GTK 3 desktop dependencies.
- [`wry-0.57.0-patched/`](#wry-android-main-thread-panics): wry with its Android main-thread message pump made
  panic-free.
- [`webrtc-sys-build-0.3.19-patched/`](#webrtc-sys-build-on-zip-9): libwebrtc's build helper on zip 9, whose archive
  extraction is not RUSTSEC-2026-0367, -0368 and -0369.

All three are wired in by `[patch.crates-io]` in the root `Cargo.toml`, and all keep the upstream version number:
they are not upstream releases.

# GLib compatibility backport

`glib/` is the unmodified crates.io source for **glib 0.18.5**, except for the
two-line fix in `src/variant_iter.rs` and `tests/variant_str_security.rs`.
Its MIT license and copyright notices are included.

Tauri's GTK 3 dependencies require glib 0.18, so adding glib 0.20 alongside it
would leave the vulnerable copy installed. The root Cargo manifest instead
patches crates.io to this local copy. The version remains 0.18.5; this is not an
upstream release.

The fix passes a mutable out-pointer to `g_variant_get_child`, matching
[upstream PR 1343](https://github.com/gtk-rs/gtk-rs-core/pull/1343), commit
`05dff0ee696f9bcd8617cd48c4b812d046d440cb`, for
[RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429.html).

Validate on Linux with GLib development headers and pkg-config installed:

```sh
cargo test --manifest-path vendor/glib/Cargo.toml --release --test variant_str_security
```

Run this in release mode: compiler optimization exposed the original bug.
Remove this patch when Tauri's GTK dependency supports a fixed upstream GLib
release, then regenerate the workspace lockfile and repeat the desktop checks.
Do not suppress the advisory merely because a Windows build does not compile GTK.

# wry Android main-thread panics

`wry-0.57.0-patched/` is the unmodified crates.io source of **wry 0.57.0** (registry checksum
`a819957a01b3119af85e638a38d242af76dbc87d130dca67bfd0441072e21ff0`, minus the `.cargo-ok` marker), except for the
edits marked `PATCH (aokie)` in `src/android/main_pipe.rs` and `src/android/mod.rs`. Its MIT and Apache-2.0
licences are included. The version stays 0.57.0; this is not an upstream release.

On Android wry answers every request on the Java main thread, inside a looper callback that the `ndk` crate wraps
in `abort_on_panic`: a panic there ends the whole process with SIGABRT. The requests that wait for an answer
(`GetWebViewVersion`, `GetUrl`, `CanGoBack`, `CanGoForward`, `GetCookies`) give up after `MAIN_PIPE_TIMEOUT`
(10 s). `tauri-runtime-wry` asks for `webview_version()` while it creates the runtime, and the main thread that has
to answer is the one still running `Activity.onCreate` and then verifying classes, so on a slow start-up (a first
launch after an install on a small emulator; seen as about 30 s of process uptime) two things went wrong, one after
the other:

1. The asker had gone, and the main thread's late `tx.send(..).unwrap()` panicked
   (`panicked at wry-0.57.0/src/android/main_pipe.rs:398:36: called Result::unwrap() on an Err value: SendError(..)`,
   then SIGABRT). The patch ignores the send error (`let _ = tx.send(..)`) for every such request.
2. That alone only moves the abort: the version query had timed out, so `tauri-runtime-wry` takes the webview
   runtime as not installed, and Tauri's setup panics with `Could not find the webview runtime` a moment later.
   The patch gives the one-off version query its own, longer wait (60 s, `WEBVIEW_VERSION_TIMEOUT`). Nothing else
   waits for that answer.

`get_webview` also looked its activity up with `.unwrap()`. A destroyed activity has no proxy any more, so it
panicked for a message still queued for that activity; it now answers `None`, which every caller already handles.
This is hardening that was not reproduced on a device.

How it was shown: a debug build whose `MainActivity.onCreate` slept 12 s after `super.onCreate` aborted with the
panic above on stock wry; with only the `send` edits it aborted with `Could not find the webview runtime`; with
all the edits it started normally. The same check is described in `apps/aokie-mobile/README.md`.

To compare with upstream, unpack the registry crate (its SHA-256 is the checksum above) and diff:

```sh
curl -L https://static.crates.io/crates/wry/wry-0.57.0.crate | tar xz
diff -r wry-0.57.0 vendor/wry-0.57.0-patched
```

`Cargo.lock` carries no `source`/`checksum` lines for `wry` while the patch is in place (as for `glib`); the checksum
above is the record of what they were.

Remove this patch when a wry release carries the same fixes, or when Tauri moves past 0.57:

1. Delete the `wry = { path = ... }` line under `[patch.crates-io]` in the root `Cargo.toml` and the
   `vendor/wry-0.57.0-patched/` directory.
2. `cargo update -p wry` (or bump `tauri` and let it pick the new wry) and check that `Cargo.lock` has the registry
   `source` and `checksum` lines for `wry` again.
3. Rebuild the Android app and repeat the 12-second main-thread stall check; it must still start without
   `panicked at` in logcat.

Upstream issue to file when it is convenient: `MainPipe::handle_message` unwraps `Sender::send` for requests whose
asker has already timed out inside a callback that aborts on panic (and `get_webview` unwraps a missing activity),
and `tauri-runtime-wry` turns a timed-out `webview_version()` into `WebviewRuntimeNotInstalled`.

# webrtc-sys-build on zip 9

`webrtc-sys-build-0.3.19-patched/` is the unmodified crates.io source of **webrtc-sys-build 0.3.19** (registry
checksum `0cd4f418e16f4b7ba028e6b5c70e4f532c7d2b5ca39726d42b699f3e4591ed5c`, from LiveKit's rust-sdks at
`2d9f01ab1e933a86a8a5c53805ee29ee58b9be1b`), minus the `.cargo-ok` marker and the crate's own `Cargo.lock` (not read
for a dependency; it named zip 0.6.6), except for the `zip` dependency in `Cargo.toml`, marked `PATCH (aokie)`. Its
Apache-2.0 licence is included as `LICENSE`, LiveKit's from that repository. The version stays 0.3.19; this is not an
upstream release.

It is the build script helper of `webrtc-sys` (through `libwebrtc`, for `aokie-media` and the plugin): at build time
it downloads LiveKit's prebuilt libwebrtc and unpacks it with `zip::ZipArchive::extract`. 0.3.19 is its latest
release and asks for zip 0.6, and `ZipArchive::extract` before zip 9 keeps SUID/SGID bits, makes symbolic links whose
targets leave the destination, and applies permissions through them (RUSTSEC-2026-0367, -0368, -0369), which failed
`cargo audit` and so the release job. The patch asks for zip 9 with deflate through flate2 on zlib-rs alone (0.6's
`deflate` was decompression only too; zip 9's defaults would add zstd, bzip2, lzma and AES). `src/lib.rs` compiles
against zip 9 unchanged. zip 9.0.5 asks for Rust 1.88, the workspace's MSRV.

Checked on Windows (2026-10-19): `aokie-media` built from an empty target folder, so the helper downloaded libwebrtc
`win-x64-release-webrtc-89d790b` and unpacked it through zip 9 (38,755 files); `cargo audit` reports no
vulnerability; the plugin's tests (`voice,managed-beta-driver`) pass.

Remove this patch when a webrtc-sys-build release asks for zip 9 or later:

1. Delete the `webrtc-sys-build = { path = ... }` line under `[patch.crates-io]` in the root `Cargo.toml` and the
   `vendor/webrtc-sys-build-0.3.19-patched/` directory.
2. `cargo update -p webrtc-sys-build` (or bump `libwebrtc`) and check that `Cargo.lock` has the registry `source` and
   `checksum` lines for it again, and no zip before 9.
3. Build `aokie-media` from an empty target folder (the helper downloads and unpacks libwebrtc there) and run
   `cargo audit`.