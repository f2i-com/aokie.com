//! winbt-hfp-enable — query/enable classic profile services on the paired phone.
//! Usage: winbt-hfp-enable <phone address>   (or AOKIE_PHONE_ADDRESS)

#[cfg(target_os = "windows")]
fn main() {
    let address = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("AOKIE_PHONE_ADDRESS").ok())
        .and_then(|a| aokie_winbt::pairing::parse_address(&a));
    let Some(address) = address else {
        eprintln!("usage: winbt-hfp-enable <phone address XX:XX:XX:XX:XX:XX> (or set AOKIE_PHONE_ADDRESS)");
        std::process::exit(2);
    };
    aokie_winbt::hfp_enable::run(address);
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("winbt-hfp-enable is Windows-only.");
}
