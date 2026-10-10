//! Build provenance (audit CROSS-OBS-001): stamp the git ref into the binary
//! so `plugin.health` / support bundles can say exactly WHICH build answered.
//! Best-effort — a non-git build (source tarball) stamps "unknown" rather
//! than failing.

fn main() {
    // One name for "this build has the radio": the systems `aokie-bluetooth` has a USB transport for (WinUSB on
    // Windows, libusb on Linux and macOS). The phone link is gated on it; code that calls Windows itself (the
    // driver install, the native Bluetooth backend, DPAPI, the 1 ms timer) stays `target_os = "windows"`.
    println!("cargo:rustc-check-cfg=cfg(aokie_radio)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(os.as_str(), "windows" | "linux" | "macos") {
        println!("cargo:rustc-cfg=aokie_radio");
    }
    // The speech libraries (sherpa-onnx, ONNX Runtime) are shared libraries that ship beside the programs. Windows
    // looks beside a program for them by itself; macOS and Linux look where the program's own run path says, so
    // the programs are given the folder they are in as one. Without it a bundle's plugin does not start there.
    match os.as_str() {
        "macos" => println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path"),
        "linux" => println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN"),
        _ => {}
    }
    let git_ref = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AOKIE_BUILD_REF={git_ref}");
    // Re-stamp when HEAD moves. .git/HEAD only changes on branch SWITCHES
    // (its content is the ref name) — commits move the branch ref file, so
    // watch that too or the stamp goes stale after every commit.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/main");
}
