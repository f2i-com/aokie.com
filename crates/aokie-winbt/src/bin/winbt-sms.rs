//! winbt-sms — native-transport bring-up + SMS verification tool.
//! Usage: winbt-sms <phone address> <to> [body]   (the address may come from AOKIE_PHONE_ADDRESS instead, and
//! then the arguments are <to> [body]). Nothing is defaulted: no phone and no number are written in here.

#[cfg(target_os = "windows")]
fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let address = match args.first().and_then(|a| aokie_winbt::pairing::parse_address(a)) {
        Some(address) => {
            args.remove(0);
            Some(address)
        }
        None => std::env::var("AOKIE_PHONE_ADDRESS")
            .ok()
            .and_then(|a| aokie_winbt::pairing::parse_address(&a)),
    };
    let (Some(address), Some(to)) = (address, args.first().cloned()) else {
        eprintln!("usage: winbt-sms <phone address XX:XX:XX:XX:XX:XX> <to> [body] (or set AOKIE_PHONE_ADDRESS)");
        std::process::exit(2);
    };
    let body = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "Aokie native Bluetooth test - the built-in Windows stack works!".to_string());
    aokie_winbt::sms_tool::run(address, &to, &body, std::time::Duration::from_secs(300));
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("winbt-sms is Windows-only.");
}
