//! aokie-driver-helper: the elevated half of the WinUSB driver install (AOK-DRIVER-001).
//!
//! Its code is Windows' own (`windows.rs`): the driver package, the certificate stores, the job a dispatcher hands
//! it. On Linux and macOS the radio stack reaches a dongle through libusb and no driver is installed, so there this
//! is a program that says so and does nothing: it exists so that the crate's programs build on every system.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
fn main() {
    windows::main()
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("aokie-driver-helper installs the WinUSB driver on Windows; this system installs no driver for the dongle");
    std::process::exit(1);
}
