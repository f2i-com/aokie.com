//! winbt-sms — native-transport bring-up + SMS verification tool.
//! Usage: winbt-sms [to] [body]   (defaults: 0421285243, test text)

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let to = args.get(1).map(String::as_str).unwrap_or("0421285243");
    let body = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("Aokie native Bluetooth test - the built-in Windows stack works!");
    aokie_winbt::sms_tool::run(
        0x04C8B0E13FF3,
        to,
        body,
        std::time::Duration::from_secs(300),
    );
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("winbt-sms is Windows-only.");
}
