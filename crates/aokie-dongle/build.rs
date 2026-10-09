//! One name for "this build has the radio".
//!
//! `aokie-bluetooth`'s host stack reaches a dongle over raw USB: WinUSB on Windows, libusb on Linux and macOS. Code
//! that only needs that stack to exist (the bridge from its events to the app's, the diagnostic tool) is gated on
//! `aokie_radio`. Code that calls Windows itself (the WinUSB driver package, its certificates, the elevated helper)
//! stays `target_os = "windows"`: the other systems install no driver.
fn main() {
    println!("cargo:rustc-check-cfg=cfg(aokie_radio)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(os.as_str(), "windows" | "linux" | "macos") {
        println!("cargo:rustc-cfg=aokie_radio");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
