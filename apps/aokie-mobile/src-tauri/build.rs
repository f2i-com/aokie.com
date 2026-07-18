fn main() {
    tauri_build::build();
    embed_comctl32_v6_in_test_binaries();
}

/// Makes `cargo test` able to LAUNCH on Windows.
///
/// This crate's linked Win32 surface imports comctl32's `TaskDialogIndirect`,
/// which only ComCtl32 v6 exports — System32's comctl32.dll is 5.82. The app
/// binary is fine because `tauri_build` links a resource carrying the
/// Common-Controls activation manifest, but the lib test harness links no such
/// resource, so the loader binds v5.82 and the binary dies at load with
/// STATUS_ENTRYPOINT_NOT_FOUND (0xC0000139) before a single test runs.
///
/// ⚠️ Two things below look redundant and are not:
///
/// 1. This uses the general `rustc-link-arg`, NOT `rustc-link-arg-tests`. The
///    latter covers `tests/*.rs` integration targets only — this crate has
///    none, so cargo rejects the directive outright ("does not have a test
///    target"), and it would not have reached the lib harness that actually
///    fails anyway.
/// 2. Bins must then opt back OUT. `tauri_build`'s resource already carries a
///    MANIFEST resource, so letting the linker embed a second one makes bin
///    targets fail to link (`CVT1100: duplicate resource. type:MANIFEST`).
///    `/MANIFEST:NO` suppresses only the linker-GENERATED manifest and leaves
///    the resource-embedded one alone, so the shipped app keeps the exact
///    manifest it has today (verified byte-identical before/after).
fn embed_comctl32_v6_in_test_binaries() {
    // MSVC linker syntax, and a Windows-only concern — Android/Linux/macOS
    // builds of this same crate must never see these flags.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("comctl32-v6.manifest");
    println!("cargo:rerun-if-changed=comctl32-v6.manifest");
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
}
