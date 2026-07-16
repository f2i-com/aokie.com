#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("aokie-radio-diag is only supported on Windows");
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
fn run() -> Result<(), String> {
    use aokie_bluetooth::aokie_radio::{manager, winusb};

    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());
    match command.as_str() {
        "help" | "--help" | "-h" => {
            print_usage();
        }
        "usb-list" => {
            for device in aokie_dongle::list_devices(true)? {
                println!(
                    "{:04x}:{:04x} driver={} composite={} desc={} hwid={}",
                    device.vid,
                    device.pid,
                    empty_dash(&device.driver),
                    device.is_composite,
                    device.description,
                    device.hardware_id
                );
            }
        }
        "interfaces" => {
            let interfaces = winusb::enumerate_radio_interfaces()?;
            if interfaces.is_empty() {
                println!("no WinUSB radio interfaces found");
            }
            for interface in interfaces {
                println!("{} {}", interface_source(interface.source), interface.path);
            }
        }
        "hci-interfaces" => {
            let interfaces = winusb::enumerate_hci_radio_interfaces()?;
            if interfaces.is_empty() {
                println!("no HCI-shaped WinUSB radio interfaces found");
            }
            for interface in interfaces {
                println!("{} {}", interface_source(interface.source), interface.path);
            }
        }
        "diagnose" => match winusb::diagnose_first_available()? {
            Some(report) => print_interface_diagnostics(&report),
            None => println!("no openable WinUSB radio interface found"),
        },
        "address" => match winusb::read_first_local_address()? {
            Some(address) => println!("{} {}", address.local_address, address.device_path),
            None => println!("no readable WinUSB radio controller found"),
        },
        "probe" => match winusb::probe_first_controller()? {
            Some(probe) => print_probe(&probe),
            None => println!("no probeable WinUSB radio controller found"),
        },
        "init" => match manager::initialize_first_controller()? {
            Some(report) => print_init(&report),
            None => println!("no initializable WinUSB radio controller found"),
        },
        "listen-events" => {
            let duration = duration_arg(args.next(), 5)?;
            let mut store = load_pairing_store()?;
            match manager::listen_first_controller(duration, &mut store)? {
                Some(report) => print_event_listen(&report),
                None => println!("no controller could run the event listener"),
            }
        }
        "listen-acl" => {
            let duration = duration_arg(args.next(), 5)?;
            match manager::listen_acl_first_controller(duration)? {
                Some(report) => print_acl_listen(&report),
                None => println!("no controller could run the ACL listener"),
            }
        }
        "runtime" => {
            let duration = duration_arg(args.next(), 5)?;
            let mut store = load_pairing_store()?;
            match manager::listen_runtime_first_controller(duration, &mut store)? {
                Some(report) => print_runtime(&report),
                None => println!("no controller could run the diagnostic runtime"),
            }
        }
        "live-runtime" => {
            // Drives the production-shape `AokieRuntime` against the
            // real dongle for [seconds]. Useful for smoke-testing
            // without booting the Tauri app: emits events to stdout
            // for the duration window. Auto-answers the FIRST incoming
            // call after a short delay so the operator can capture a
            // full call-handle cycle (Connection_Complete → SLC →
            // SCO → Disconnection_Complete) in one run; subsequent
            // calls during the same window observe-only. R6/P2-8
            // doc-drift fix: the previous comment said "answers
            // nothing automatically" while the code below explicitly
            // auto-answers the first call — wording now matches the
            // actual behaviour.
            let duration = duration_arg(args.next(), 10)?;
            run_live_runtime(duration)?;
        }
        other => {
            return Err(format!("unknown command '{other}'"));
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_live_runtime(duration: std::time::Duration) -> Result<(), String> {
    use aokie_bluetooth::aokie_radio::pairing_store::default_store_path;
    use aokie_bluetooth::aokie_radio::runtime::{AokieRuntime, RuntimeEvent};
    use std::time::Instant;

    let pairing_dir = aokie_core::paths::app_data_dir()
        .ok_or_else(|| "could not resolve user data dir".to_string())?;
    let mut runtime = AokieRuntime::start(default_store_path(&pairing_dir))?;
    println!(
        "[live-runtime] started; observing for {} s",
        duration.as_secs()
    );

    let deadline = Instant::now() + duration;
    let mut answered_call = false;
    while Instant::now() < deadline {
        while let Some(event) = runtime.try_recv_event() {
            println!("[live-runtime] event: {:?}", event);
            if matches!(event, RuntimeEvent::CallIncoming) && !answered_call {
                answered_call = true;
                println!("[live-runtime] auto-answering incoming call");
                if let Err(e) = runtime.answer() {
                    eprintln!("[live-runtime] answer failed: {}", e);
                }
            }
        }
        let mut audio_count = 0usize;
        while runtime.try_recv_audio().is_some() {
            audio_count += 1;
        }
        if audio_count > 0 {
            println!("[live-runtime] drained {} audio frames", audio_count);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    println!(
        "[live-runtime] done — initialized={} connected={} call_active={} sample_rate={} local={} remote={}",
        runtime.is_initialized(),
        runtime.is_connected(),
        runtime.is_call_active(),
        runtime.sample_rate(),
        runtime.local_address(),
        runtime.remote_address(),
    );
    runtime.shutdown();
    Ok(())
}

#[cfg(target_os = "windows")]
fn print_usage() {
    println!("Usage: aokie-radio-diag <command> [seconds]");
    println!("Commands:");
    println!("  usb-list        List present USB devices from the Aokie catalog");
    println!("  interfaces      List WinUSB radio interface paths");
    println!("  hci-interfaces  List openable WinUSB interfaces with HCI event/ACL pipes");
    println!("  diagnose        Open first radio interface and print pipe diagnostics");
    println!("  address         Reset first controller and read BD_ADDR");
    println!("  probe           Reset first controller and read version/features/buffers");
    println!("  init            Run AokieRadio controller initialization");
    println!("  listen-events   Run pairing/HCI event listener for [seconds], default 5");
    println!("  listen-acl      Run ACL/L2CAP/HFP listener for [seconds], default 5");
    println!("  runtime         Run combined diagnostic runtime for [seconds], default 5");
    println!("  live-runtime    Drive AokieRuntime for [seconds] (auto-answers first call)");
}

#[cfg(target_os = "windows")]
fn duration_arg(
    value: Option<String>,
    default_seconds: u64,
) -> Result<std::time::Duration, String> {
    let seconds = match value {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| format!("invalid seconds value '{value}'"))?,
        None => default_seconds,
    };
    Ok(std::time::Duration::from_secs(seconds.clamp(1, 60)))
}

#[cfg(target_os = "windows")]
fn load_pairing_store(
) -> Result<aokie_bluetooth::aokie_radio::pairing_store::AokiePairingStore, String> {
    let data_dir = aokie_core::paths::app_data_dir()
        .ok_or_else(|| "could not resolve Windows app data directory".to_string())?;
    let path = aokie_bluetooth::aokie_radio::pairing_store::default_store_path(&data_dir);
    aokie_bluetooth::aokie_radio::pairing_store::AokiePairingStore::load(path)
}

#[cfg(target_os = "windows")]
fn interface_source(source: aokie_bluetooth::aokie_radio::winusb::InterfaceSource) -> &'static str {
    match source {
        aokie_bluetooth::aokie_radio::winusb::InterfaceSource::AokieWinUsb => "aokie-winusb",
        aokie_bluetooth::aokie_radio::winusb::InterfaceSource::GenericUsbDevice => "usb-device",
        aokie_bluetooth::aokie_radio::winusb::InterfaceSource::RealtekWinUsb => "realtek-winusb",
    }
}

#[cfg(target_os = "windows")]
fn pipe_kind(kind: aokie_bluetooth::aokie_radio::winusb::PipeKind) -> String {
    match kind {
        aokie_bluetooth::aokie_radio::winusb::PipeKind::Bulk => "bulk".to_string(),
        aokie_bluetooth::aokie_radio::winusb::PipeKind::Interrupt => "interrupt".to_string(),
        aokie_bluetooth::aokie_radio::winusb::PipeKind::Isochronous => "isochronous".to_string(),
        aokie_bluetooth::aokie_radio::winusb::PipeKind::Control => "control".to_string(),
        aokie_bluetooth::aokie_radio::winusb::PipeKind::Unknown(value) => {
            format!("unknown({value})")
        }
    }
}

#[cfg(target_os = "windows")]
fn pipe_direction(direction: aokie_bluetooth::aokie_radio::winusb::PipeDirection) -> &'static str {
    match direction {
        aokie_bluetooth::aokie_radio::winusb::PipeDirection::In => "in",
        aokie_bluetooth::aokie_radio::winusb::PipeDirection::Out => "out",
    }
}

#[cfg(target_os = "windows")]
fn print_interface_diagnostics(
    report: &aokie_bluetooth::aokie_radio::winusb::InterfaceDiagnostics,
) {
    println!("path: {}", report.device_path);
    println!(
        "control interface={} alt={}",
        report.interface_number, report.alternate_setting
    );
    println!("pipes:");
    for pipe in &report.pipes {
        println!(
            "  alt={} id=0x{:02x} {} {} max={} interval={}",
            pipe.alternate_setting,
            pipe.id,
            pipe_direction(pipe.direction),
            pipe_kind(pipe.kind),
            pipe.max_packet_size,
            pipe.interval
        );
    }
    println!("classified: {:?}", report.classified);
}

#[cfg(target_os = "windows")]
fn print_probe(probe: &aokie_bluetooth::aokie_radio::winusb::ControllerProbe) {
    println!("path: {}", probe.device_path);
    println!("local address: {}", probe.local_address);
    println!(
        "version: hci={} rev=0x{:04x} lmp={} manufacturer={} subversion=0x{:04x}",
        probe.version.hci_version,
        probe.version.hci_revision,
        probe.version.lmp_pal_version,
        probe.version.manufacturer_name,
        probe.version.lmp_pal_subversion
    );
    println!("features: {:02x?}", probe.features);
    println!(
        "buffers: acl_len={} sco_len={} acl_packets={} sco_packets={}",
        probe.buffer_size.acl_data_packet_length,
        probe.buffer_size.sco_data_packet_length,
        probe.buffer_size.total_num_acl_data_packets,
        probe.buffer_size.total_num_sco_data_packets
    );
}

#[cfg(target_os = "windows")]
fn print_init(report: &aokie_bluetooth::aokie_radio::manager::ControllerInitReport) {
    println!("initialized: {}", report.device_path);
    println!("local address: {}", report.local_address);
    println!("local name: {}", report.local_name);
    println!("class: 0x{:06x}", report.class_of_device);
    println!("voice setting: 0x{:04x}", report.voice_setting);
    println!("scan enable: 0x{:02x}", report.scan_enable);
    println!("simple pairing: {}", report.simple_pairing_enabled);
}

#[cfg(target_os = "windows")]
fn print_event_listen(report: &aokie_bluetooth::aokie_radio::manager::ControllerListenReport) {
    print_init(&report.init);
    println!("events: {}", report.events.len());
    for event in &report.events {
        print_event_record(event);
    }
    println!(
        "pairing store: {} keys at {}",
        report.stored_link_keys, report.pairing_store_path
    );
}

#[cfg(target_os = "windows")]
fn print_acl_listen(report: &aokie_bluetooth::aokie_radio::manager::ControllerAclListenReport) {
    print_init(&report.init);
    println!("acl exchanges: {}", report.acl_exchanges.len());
    for (index, exchange) in report.acl_exchanges.iter().enumerate() {
        print_acl_exchange(index, exchange);
    }
    println!("hfp events: {}", report.hfp_events.len());
    for event in &report.hfp_events {
        print_hfp_event(event);
    }
}

#[cfg(target_os = "windows")]
fn print_runtime(report: &aokie_bluetooth::aokie_radio::manager::ControllerRuntimeListenReport) {
    print_init(&report.init);
    println!("hci events: {}", report.events.len());
    for event in &report.events {
        print_event_record(event);
    }
    println!("acl exchanges: {}", report.acl_exchanges.len());
    for (index, exchange) in report.acl_exchanges.iter().enumerate() {
        print_acl_exchange(index, exchange);
    }
    println!("hfp events: {}", report.hfp_events.len());
    for event in &report.hfp_events {
        print_hfp_event(event);
    }
    println!(
        "hfp control: auto_answer={} attempts={} packets_sent={} last={}",
        report.hfp_control.auto_answer_enabled,
        report.hfp_control.answer_attempts,
        report.hfp_control.answer_packets_sent,
        report.hfp_control.last_action.as_deref().unwrap_or("-")
    );
    println!(
        "sco audio: packets={} payload_bytes={} bad={} active={:?}",
        report.sco_audio.packets,
        report.sco_audio.payload_bytes,
        report.sco_audio.bad_packets,
        report.sco_audio.active_connection_handle
    );
    println!("sco transport: {:?}", report.sco_transport);
    println!("sco pcm: {:?}", report.sco_pcm);
    println!("sco tx: {:?}", report.sco_tx);
    println!(
        "pairing store: {} keys at {}",
        report.stored_link_keys, report.pairing_store_path
    );
}

#[cfg(target_os = "windows")]
fn print_event_record(event: &aokie_bluetooth::aokie_radio::manager::ControllerEventRecord) {
    println!(
        "  0x{:02x} {}: {}{}",
        event.event_code,
        event.name,
        event.summary,
        event
            .action
            .as_ref()
            .map(|action| format!(" action={action}"))
            .unwrap_or_default()
    );
}

#[cfg(target_os = "windows")]
fn print_acl_exchange(
    index: usize,
    exchange: &aokie_bluetooth::aokie_radio::manager::AclExchangeRecord,
) {
    println!(
        "  ACL #{index}: inbound={} responses={} lengths={:?}",
        exchange.inbound_len, exchange.responses_sent, exchange.response_lengths
    );
}

#[cfg(target_os = "windows")]
fn print_hfp_event(event: &aokie_bluetooth::aokie_radio::manager::HfpEventRecord) {
    println!("  HFP {}: {}", event.name, event.summary);
}

#[cfg(target_os = "windows")]
fn empty_dash(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}
