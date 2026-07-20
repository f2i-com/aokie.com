//! Interactive bring-up/verification tool for the native transport: keep
//! attempting the MAP link to the phone (each attempt makes Windows page it)
//! and, once the link is up, send one SMS through the production `MapLoop`
//! path (`BtChannel` → OBEX CONNECT → SETPATH outbox → PUT bMessage). Not a
//! test double — the same code the worker drives.

use std::sync::atomic::Ordering;
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aokie_dongle::bluetooth::BluetoothEvent;

use crate::map::MapLoop;
use crate::runtime::NativeShared;

/// Retry the MAP link for up to `max_wait`, then send one SMS. Prints every
/// attempt and every engine event so the live bring-up is observable.
pub fn run(phone_address: u64, to: &str, body: &str, max_wait: Duration) {
    let (event_tx, event_rx) = channel::<BluetoothEvent>();
    std::thread::spawn(move || {
        while let Ok(ev) = event_rx.recv() {
            match ev {
                BluetoothEvent::SmsSent { .. } | BluetoothEvent::SmsSendFailed { .. } => {}
                other => println!("[event] {other:?}"),
            }
        }
    });

    let shared = Arc::new(NativeShared::default());
    shared.connected.store(true, Ordering::Relaxed);
    if let Ok(mut addr) = shared.phone_address.write() {
        *addr = phone_address;
    }
    let mut map = MapLoop::new(event_tx, shared);

    let msg_id = format!(
        "winbt-sms-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let start = Instant::now();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        print!("[attempt {attempt}] opening MAP MAS + PUT bMessage… ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        match map.send_sms(&msg_id, to, body, None) {
            Ok(()) => {
                println!("OK — SMS accepted by the phone (messageId {msg_id})");
                break;
            }
            Err(e) => println!("failed: {e}"),
        }
        if start.elapsed() >= max_wait {
            println!("gave up after {attempt} attempts ({:?})", start.elapsed());
            return;
        }
        std::thread::sleep(Duration::from_secs(6));
    }
    // Let the SmsSent event + any immediate inbound traffic print.
    std::thread::sleep(Duration::from_secs(3));
}
