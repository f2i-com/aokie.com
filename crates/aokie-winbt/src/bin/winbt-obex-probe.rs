//! winbt-obex-probe — raw OBEX CONNECT probe against the paired phone.

#[cfg(target_os = "windows")]
fn main() {
    aokie_winbt::obex_probe::run(0x04C8B0E13FF3);
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("winbt-obex-probe is Windows-only.");
}
