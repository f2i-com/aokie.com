//! Live check of the software dongle reset, on the real dongle.
//!
//! Run it only while the Aokie plugin is stopped (WinUSB lets one process
//! own the dongle):
//!
//!     cargo run -p aokie-bluetooth --example transport_reset_live -- <phone BD_ADDR> [cycles]
//!
//! It opens the dongle through the production runtime (the same code the
//! plugin runs), pages the bonded phone like the plugin's auto-connect,
//! then calls `reset_transport()` — close the WinUSB handle, reopen it,
//! HCI_Reset, re-initialise — and waits for the controller and the phone
//! to come back. It never answers, places or ends a call, and stops at
//! once if a call starts ringing.

#[cfg(target_os = "windows")]
fn main() {
    use aokie_bluetooth::aokie_radio::pairing_store::default_store_path;
    use aokie_bluetooth::aokie_radio::runtime::{AokieRuntime, RuntimeEvent};
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let phone = args
        .next()
        .expect("usage: transport_reset_live <phone BD_ADDR> [cycles]");
    let cycles: u32 = args.next().and_then(|c| c.parse().ok()).unwrap_or(1);
    let data_dir = aokie_core::paths::app_data_dir().expect("no app data dir");
    let store = default_store_path(&data_dir);
    println!("[live] pairing store: {}", store.display());

    let started = Instant::now();
    let stamp = move || format!("{:>7.2}s", started.elapsed().as_secs_f64());
    let mut rt = AokieRuntime::start_with_options(store, None).expect("start runtime");

    // Wait for `want` (checked on every event), printing everything seen.
    // Returns false on timeout or when a call starts (the test stops).
    let wait_for = |rt: &mut AokieRuntime,
                    what: &str,
                    secs: u64,
                    want: &dyn Fn(&RuntimeEvent) -> bool|
     -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            while let Some(ev) = rt.try_recv_event() {
                println!("[live] {} event: {:?}", stamp(), ev);
                if matches!(
                    ev,
                    RuntimeEvent::CallIncoming
                        | RuntimeEvent::CallRinging
                        | RuntimeEvent::OutgoingDialing
                ) {
                    println!("[live] a call started - stopping the test without touching it");
                    return false;
                }
                if want(&ev) {
                    println!("[live] {} OK: {}", stamp(), what);
                    return true;
                }
            }
            while rt.try_recv_audio().is_some() {}
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("[live] {} TIMEOUT waiting for: {}", stamp(), what);
        false
    };

    let initialized = |ev: &RuntimeEvent| matches!(ev, RuntimeEvent::Initialized(_));
    let connected = |ev: &RuntimeEvent| matches!(ev, RuntimeEvent::DeviceConnected(_));
    let disconnected = |ev: &RuntimeEvent| matches!(ev, RuntimeEvent::DeviceDisconnected(_));

    if !wait_for(&mut rt, "controller initialised", 30, &initialized) {
        std::process::exit(1);
    }
    std::thread::sleep(Duration::from_secs(4));
    match rt.connect(phone.clone()) {
        Ok(started) => println!("[live] {} page to {} started={}", stamp(), phone, started),
        Err(e) => println!("[live] {} page to {} refused: {}", stamp(), phone, e),
    }
    if !wait_for(&mut rt, "phone connected", 60, &connected) {
        std::process::exit(2);
    }
    // Let the profiles (HFP SLC, MAP) settle before pulling the rug.
    let _ = wait_for(&mut rt, "settle", 20, &|_| false);

    for cycle in 1..=cycles {
        println!(
            "[live] {} cycle {}/{}: reset_transport()",
            stamp(),
            cycle,
            cycles
        );
        rt.reset_transport().expect("reset_transport");
        if !wait_for(&mut rt, "phone dropped by the reset", 15, &disconnected) {
            std::process::exit(3);
        }
        if !wait_for(&mut rt, "controller re-initialised", 60, &initialized) {
            std::process::exit(4);
        }
        if !wait_for(&mut rt, "phone reconnected", 100, &connected) {
            std::process::exit(5);
        }
        let _ = wait_for(&mut rt, "settle", 20, &|_| false);
        println!(
            "[live] {} cycle {} done: initialized={} connected={}",
            stamp(),
            cycle,
            rt.is_initialized(),
            rt.is_connected()
        );
    }
    println!("[live] {} all cycles passed; shutting down", stamp());
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("transport_reset_live runs on Windows (WinUSB) only");
}
