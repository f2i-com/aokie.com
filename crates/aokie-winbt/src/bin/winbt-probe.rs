//! winbt-probe — Phase-0 capability probe for the native Bluetooth backend.
//! Read-only: no pairing, no registration, no streaming.

#[cfg(target_os = "windows")]
fn main() {
    aokie_winbt::probe::run();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("winbt-probe is Windows-only (it probes the built-in Windows Bluetooth stack).");
}
