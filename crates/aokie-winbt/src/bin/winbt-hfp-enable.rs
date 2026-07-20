//! winbt-hfp-enable — query/enable classic profile services on the paired phone.

#[cfg(target_os = "windows")]
fn main() {
    aokie_winbt::hfp_enable::run(0x04C8B0E13FF3);
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("winbt-hfp-enable is Windows-only.");
}
