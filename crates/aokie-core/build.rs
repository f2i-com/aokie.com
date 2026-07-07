// aokie-core build script.
//
// `paths::BUNDLE_IDENTIFIER` resolves `env!("AOKIE_BUNDLE_IDENTIFIER")`
// at compile time — in the legacy Tauri app the identifier was bridged
// out of `tauri.conf.json` by that crate's build.rs. This crate is
// Tauri-free and has no `tauri.conf.json`, so we set the same value
// (`com.aokie.app`, the legacy app's bundle id) directly. Keeping the
// identical identifier means these crates resolve to the SAME
// `<RoamingAppData>/com.aokie.app` data dir the legacy app uses, so a
// side-by-side install reads/writes one store rather than two.
//
// An `AOKIE_BUNDLE_IDENTIFIER` already present in the environment wins,
// so a downstream app fork can override the data-dir name without
// editing this crate.

const BUNDLE_IDENTIFIER_FALLBACK: &str = "com.aokie.app";

fn main() {
    println!("cargo:rerun-if-env-changed=AOKIE_BUNDLE_IDENTIFIER");
    let identifier = std::env::var("AOKIE_BUNDLE_IDENTIFIER")
        .unwrap_or_else(|_| BUNDLE_IDENTIFIER_FALLBACK.to_string());
    println!("cargo:rustc-env=AOKIE_BUNDLE_IDENTIFIER={}", identifier);
}
