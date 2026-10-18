//! winbt-obex-probe — raw OBEX CONNECT probe against the paired phone.
//! Usage: winbt-obex-probe <phone address>   (or AOKIE_PHONE_ADDRESS)

#[cfg(target_os = "windows")]
fn main() {
    let address = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("AOKIE_PHONE_ADDRESS").ok())
        .and_then(|a| aokie_winbt::pairing::parse_address(&a));
    let Some(address) = address else {
        eprintln!("usage: winbt-obex-probe <phone address XX:XX:XX:XX:XX:XX> (or set AOKIE_PHONE_ADDRESS)");
        std::process::exit(2);
    };
    aokie_winbt::obex_probe::run(address);
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("winbt-obex-probe is Windows-only.");
}
