//! Does this computer reach a USB Bluetooth dongle through Aokie's own transport?
//!
//!     cargo run -p aokie-bluetooth --example dongle_probe
//!
//! It lists the USB Bluetooth controllers the transport finds (WinUSB on Windows, libusb on Linux and macOS), says
//! which endpoints each has (events, data, and the two a call's audio needs), then opens each one, resets it and
//! asks it who it is. Nothing is paired and nothing is stored. A controller another program holds (the system's own
//! Bluetooth, or a running Aokie) cannot be opened, and the line says why: on a Mac that is the answer to whether
//! macOS leaves a dongle to a program that drives it itself.
//!
//! The reset is the controller's own (HCI Reset): do not run this against a dongle that is carrying a call.

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn main() {
    use aokie_bluetooth::aokie_radio::transport as radio;

    let found = match radio::enumerate_hci_radio_interfaces() {
        Ok(found) => found,
        Err(e) => {
            println!("the USB devices could not be listed: {e}");
            std::process::exit(2);
        }
    };
    println!("{} USB Bluetooth controller(s) found", found.len());
    let mut opened = 0;
    for iface in &found {
        println!("- {}", iface.path);
        match radio::diagnose_interface_path(&iface.path) {
            Ok(d) => {
                let has = |pipe: &Option<radio::PipeInfo>| if pipe.is_some() { "yes" } else { "NO" };
                let c = &d.classified;
                println!(
                    "  interface {}: events {}, data in {} and out {}, call audio in {} and out {} ({} endpoints over its settings)",
                    d.interface_number,
                    has(&c.event_in),
                    has(&c.acl_in),
                    has(&c.acl_out),
                    has(&c.sco_in),
                    has(&c.sco_out),
                    d.pipes.len()
                );
            }
            Err(e) => println!("  its endpoints could not be read: {e}"),
        }
        match radio::probe_controller(&iface.path) {
            Ok(p) => {
                opened += 1;
                // (the last two bytes of its address only: enough to tell two dongles apart in a report)
                let tail = p.local_address.rsplit(':').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(":");
                println!("  opened, reset and answering: {:?}", p.version);
                println!("  address ..:{tail}, buffers {:?}", p.buffer_size);
            }
            Err(e) => println!("  it could not be opened: {e}"),
        }
    }
    if found.is_empty() {
        println!("(no dongle plugged in, or the system shows it to no program)");
    }
    std::process::exit(if opened > 0 { 0 } else { 1 });
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn main() {
    println!("Aokie's radio transport is for Windows, Linux and macOS");
}
