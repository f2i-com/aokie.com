//! The libusb transport against the stand-in dongle of
//! `scripts/virtual-dongle` (a USB Bluetooth controller served over
//! USB/IP, attached by Linux's `vhci-hcd`).
//!
//! It is the one place the transport's USB code runs with no hardware:
//! taking the device from the system's driver, the bring-up commands,
//! events cut into USB packets, polls that find nothing, ACL data, the
//! voice interface's alternate settings and isochronous transfers both
//! ways. It proves that plumbing. It cannot prove call audio: the
//! stand-in's frames are timed by a program's sleep, and nothing is on
//! the air.
//!
//! The test is ignored unless asked for, and then runs only against the
//! device named in `AOKIE_VIRTUAL_DONGLE` (`usb:<bus>:<address>`, set by
//! `scripts/virtual-dongle/run.sh`), and only if that device says it is
//! the stand-in: a real dongle is never sent the stand-in's commands.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use aokie_bluetooth::aokie_radio::transport::{self, AokieHciTransport};

/// The stand-in's address (`dongle.py`): locally administered, so no real
/// controller has it.
const STAND_IN: &str = "02:A0:C1:E0:00:01";
/// The stand-in's voice streams count up modulo this.
const VOICE_MODULUS: usize = 251;

/// What the stand-in's counters say (its command 0xfc04).
#[derive(Debug)]
struct Counters {
    event_reads_taken_back: u32,
    acl_reads_taken_back: u32,
    voice_in_taken_back: u32,
    voice_out_bytes: u32,
    voice_out_breaks: u32,
    voice_alt: u8,
}

fn vendor(transport: &AokieHciTransport, ocf: u8, params: &[u8]) -> Vec<u8> {
    let mut command = vec![ocf, 0xfc, params.len() as u8];
    command.extend_from_slice(params);
    transport
        .command_return_params(&command, 0xfc00 | ocf as u16)
        .unwrap_or_else(|e| panic!("vendor command 0x{:02x}: {}", ocf, e))
}

fn counters(transport: &AokieHciTransport) -> Counters {
    let raw = vendor(transport, 0x04, &[]);
    assert_eq!(raw.len(), 26, "counters: {:02x?}", raw);
    let word = |i: usize| u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]);
    Counters {
        event_reads_taken_back: word(1),
        acl_reads_taken_back: word(5),
        voice_in_taken_back: word(9),
        voice_out_bytes: word(17),
        voice_out_breaks: word(21),
        voice_alt: raw[25],
    }
}

/// An "ACL packet" of `len` bytes: a header that says so, and bytes that
/// say where they are.
fn acl_packet(len: usize) -> Vec<u8> {
    let payload = len - 4;
    let mut packet = vec![0x01, 0x20, payload as u8, (payload >> 8) as u8];
    packet.extend((0..payload).map(|i| (i * 7 + 3) as u8));
    packet
}

/// Poll the ACL pipe as the runtime does (a few ms at a time) until `len`
/// bytes have come. Returns them and how many polls found nothing.
fn read_acl_bytes(transport: &AokieHciTransport, len: usize) -> (Vec<u8>, usize) {
    let mut bytes = Vec::new();
    let mut empty = 0;
    let deadline = Instant::now() + Duration::from_secs(3);
    while bytes.len() < len {
        assert!(
            Instant::now() < deadline,
            "ACL echo did not come ({} of {})",
            bytes.len(),
            len
        );
        match transport.read_acl(1024) {
            Ok(chunk) => bytes.extend(chunk),
            Err(e) if e.contains("libusb timeout") => empty += 1,
            Err(e) => panic!("read_acl: {}", e),
        }
    }
    (bytes, empty)
}

/// Check that `bytes` carries on the stand-in's counting stream from
/// `start`, with nothing lost, repeated or shifted.
fn assert_counts_up(bytes: &[u8], start: usize, what: &str) {
    for (i, &byte) in bytes.iter().enumerate() {
        assert_eq!(
            byte as usize,
            (start + i) % VOICE_MODULUS,
            "{}: byte {} of {} is out of sequence",
            what,
            i,
            bytes.len()
        );
    }
}

/// A voice link for `run`: read what comes in while writing packets of
/// `packet_len` bytes at the pace a call would. Returns the bytes read
/// and the bytes written.
fn voice_for(
    transport: &mut AokieHciTransport,
    run: Duration,
    packet_len: usize,
    packet_every: Duration,
) -> (Vec<u8>, usize) {
    let mut heard = Vec::new();
    let mut written = 0usize;
    let started = Instant::now();
    let mut next_write = started;
    while started.elapsed() < run {
        heard.extend(
            transport
                .read_sco(255)
                .unwrap_or_else(|e| panic!("read_sco: {}", e)),
        );
        while Instant::now() >= next_write {
            let packet: Vec<u8> = (0..packet_len)
                .map(|i| ((written + i) % VOICE_MODULUS) as u8)
                .collect();
            transport
                .write_sco(&packet)
                .unwrap_or_else(|e| panic!("write_sco: {}", e));
            written += packet_len;
            next_write += packet_every;
        }
    }
    (heard, written)
}

#[test]
#[ignore = "needs the stand-in dongle: scripts/virtual-dongle/run.sh"]
fn the_libusb_transport_against_the_stand_in_dongle() {
    let path = std::env::var("AOKIE_VIRTUAL_DONGLE")
        .expect("AOKIE_VIRTUAL_DONGLE names the stand-in (usb:<bus>:<address>)");

    // ---- found, and described from its descriptors alone ----------------
    let found = transport::enumerate_hci_radio_interfaces().expect("enumerate");
    assert!(
        found.iter().any(|interface| interface.path == path),
        "{} is not among the Bluetooth controllers libusb sees: {:?}",
        path,
        found
    );
    let described = transport::diagnose_interface_path(&path).expect("diagnose");
    assert_eq!(described.interface_number, 0);
    assert_eq!(described.classified.event_in.unwrap().id, 0x81);
    assert_eq!(described.classified.event_in.unwrap().max_packet_size, 16);
    assert_eq!(described.classified.acl_in.unwrap().id, 0x82);
    assert_eq!(described.classified.acl_out.unwrap().id, 0x02);
    // The voice endpoints of the second interface, in all six settings.
    let voice_sizes: Vec<u16> = described
        .pipes
        .iter()
        .filter(|pipe| pipe.id == 0x83)
        .map(|pipe| pipe.max_packet_size)
        .collect();
    assert_eq!(voice_sizes, vec![0, 9, 17, 25, 33, 49]);

    // ---- opened (from the system's driver, if it had it) and brought up -
    let mut radio = AokieHciTransport::open(&path).expect("open");
    radio.flush_in_pipes().expect("flush");
    radio.reset().expect("HCI Reset");
    let address = radio.read_bd_addr().expect("Read BD_ADDR");
    assert_eq!(
        address, STAND_IN,
        "this is not the stand-in dongle: nothing more is sent to it"
    );
    let version = radio.read_local_version().expect("version");
    assert_eq!(version.manufacturer_name, 0xffff);
    assert_eq!(radio.read_local_supported_features().expect("features")[0], 0xff);
    let buffers = radio.read_buffer_size().expect("buffer size");
    assert_eq!(buffers.acl_data_packet_length, 1021);
    assert_eq!(buffers.sco_data_packet_length, 64);
    // The transfers taken back so far are the system driver's own (it
    // had the dongle before us and gave its reads up when we took it).
    let at_start = counters(&radio);

    // ---- an event of exactly two USB packets ----------------------------
    // 32 bytes: no short packet ends it. It must come whole and at once,
    // not wait for the event after it.
    let asked = Instant::now();
    let wide = vendor(&radio, 0x01, &[]);
    assert_eq!(wide.len(), 27);
    assert_eq!(wide[0], 0);
    assert_eq!(&wide[1..], (1..27).collect::<Vec<u8>>().as_slice());
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "a 32-byte event was held back: {:?}",
        asked.elapsed()
    );

    // ---- events back to back ahead of a Command Complete ----------------
    // The command returns; the three that came first are handed out
    // afterwards, whole and in order (one of them exactly one packet long).
    assert_eq!(vendor(&radio, 0x02, &[]), vec![0]);
    assert_eq!(
        radio.read_event().expect("first deferred"),
        vec![0x13, 5, 1, 0x01, 0x00, 0x02, 0x00]
    );
    let mut sixteen = vec![0xff, 14];
    sixteen.extend(0..14u8);
    assert_eq!(radio.read_event().expect("second deferred"), sixteen);
    assert_eq!(
        radio.read_event().expect("third deferred"),
        vec![0x1b, 3, 0x01, 0x00, 0x05]
    );

    // ---- an event that arrives in pieces, polled as the runtime polls ---
    // The first packet comes 30 ms after the command, the rest 60 ms
    // later. Polls of 5 ms find nothing many times over, and no byte of
    // the event may be lost to them.
    assert_eq!(vendor(&radio, 0x03, &[]), vec![0]);
    radio.set_read_timeouts(5, Some(5), Some(5)).expect("timeouts");
    let mut empty_polls = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    let slow = loop {
        assert!(Instant::now() < deadline, "the slow event never came");
        match radio.read_event() {
            Ok(event) => break event,
            Err(e) if e.contains("libusb timeout") => empty_polls += 1,
            Err(e) => panic!("read_event: {}", e),
        }
    };
    let mut expected = vec![0xff, 38];
    expected.extend(100..138u8);
    assert_eq!(slow, expected);
    assert!(
        empty_polls >= 5,
        "the polls were meant to run dry while the event came in pieces ({} did)",
        empty_polls
    );
    println!("a 40-byte event in three packets over 90 ms: whole, after {empty_polls} polls that found nothing");

    // ---- ACL data, held back as a slow link would -----------------------
    radio.set_read_timeouts(1000, Some(5), Some(5)).expect("timeouts");
    assert_eq!(vendor(&radio, 0x05, &[20]), vec![0]);
    // 139 bytes is three USB packets (64 + 64 + 11): the size whose
    // middle was lost to a cancelled read on the WinUSB side.
    for len in [139usize, 128, 1025, 5] {
        let packet = acl_packet(len);
        radio.write_acl(&packet).expect("write_acl");
        let (echo, empty) = read_acl_bytes(&radio, len);
        assert_eq!(echo, packet, "the {}-byte packet came back changed", len);
        if len >= 128 {
            assert!(empty >= 2, "polls were meant to run dry ({} did)", empty);
        }
        println!("ACL: {len} bytes there and back unchanged, after {empty} polls that found nothing");
    }

    // None of those dry polls took a transfer back from the dongle: that
    // is the whole point of leaving the reads queued.
    let after_polls = counters(&radio);
    println!(
        "reads the dongle saw taken back: events {} (before us: {}), ACL {} (before us: {})",
        after_polls.event_reads_taken_back,
        at_start.event_reads_taken_back,
        after_polls.acl_reads_taken_back,
        at_start.acl_reads_taken_back
    );
    assert_eq!(
        after_polls.event_reads_taken_back, at_start.event_reads_taken_back,
        "{:?}",
        after_polls
    );
    assert_eq!(
        after_polls.acl_reads_taken_back, at_start.acl_reads_taken_back,
        "{:?}",
        after_polls
    );
    assert_eq!(after_polls.voice_alt, 0);

    // ---- a voice link: 16-bit CVSD rides setting 2 ----------------------
    assert!(radio.supports_msbc_alt_setting());
    assert!(radio.read_sco(255).is_err(), "no voice link is up yet");
    radio
        .configure_sco_alt_setting(0x0060, 1)
        .expect("configure CVSD");
    let config = radio.sco_transport_config().expect("a voice config");
    assert_eq!(config.alternate_setting, 2);
    assert_eq!(config.max_packet_size, Some(17));
    assert_eq!(config.in_pipe_id, Some(0x83));
    assert_eq!(config.out_pipe_id, Some(0x03));

    // 48 bytes of audio and a 3-byte header every 3 ms, for 400 ms.
    let (heard, written) = voice_for(
        &mut radio,
        Duration::from_millis(400),
        51,
        Duration::from_millis(3),
    );
    // In: one unbroken stream from its first byte, at about 17 bytes a
    // millisecond (the stand-in's clock is a sleep: a wide margin).
    assert_counts_up(&heard, 0, "voice in (setting 2)");
    assert!(
        heard.len() >= 17 * 200,
        "voice in carried only {} bytes in 400 ms",
        heard.len()
    );
    // A packet too big for the ring's frames is refused, not cut.
    assert!(radio.write_sco(&[0u8; 200]).is_err());
    // Out: let the ring empty, then ask the dongle what it got.
    std::thread::sleep(Duration::from_millis(120));
    let _ = radio.read_sco(255).expect("read_sco");
    let during = counters(&radio);
    assert_eq!(during.voice_alt, 2);
    assert_eq!(during.voice_out_bytes as usize, written, "{:?}", during);
    assert_eq!(during.voice_out_breaks, 0, "{:?}", during);
    println!(
        "voice on setting 2 for 400 ms: {} bytes in, in sequence; {} bytes out, all received in sequence",
        heard.len(),
        written
    );

    // ---- the link ends: the ring comes back, the interface goes quiet ---
    radio.disable_sco_alt_setting().expect("disable");
    assert!(radio.sco_transport_config().is_none());
    let after = counters(&radio);
    assert_eq!(after.voice_alt, 0);
    assert!(
        after.voice_in_taken_back > at_start.voice_in_taken_back,
        "the inbound ring's transfers were to be taken back: {:?}",
        after
    );
    println!(
        "the link ended: {} inbound voice transfers taken back from the dongle",
        after.voice_in_taken_back - at_start.voice_in_taken_back
    );
    // Ending a link that is not up is fine.
    radio.disable_sco_alt_setting().expect("disable twice");

    // ---- a second link: wide-band speech rides setting 1 ----------------
    radio
        .configure_sco_alt_setting(0x0043, 1)
        .expect("configure mSBC");
    let config = radio.sco_transport_config().expect("a voice config");
    assert_eq!(config.alternate_setting, 1);
    assert_eq!(config.max_packet_size, Some(9));
    // 24 bytes and a header every 3 ms.
    let (heard, written) = voice_for(
        &mut radio,
        Duration::from_millis(250),
        27,
        Duration::from_millis(3),
    );
    assert_counts_up(&heard, 0, "voice in (setting 1)");
    assert!(heard.len() >= 9 * 100, "only {} bytes", heard.len());
    std::thread::sleep(Duration::from_millis(120));
    let during = counters(&radio);
    assert_eq!(during.voice_alt, 1);
    assert_eq!(during.voice_out_bytes as usize, written, "{:?}", during);
    assert_eq!(during.voice_out_breaks, 0, "{:?}", during);
    println!(
        "voice on setting 1 for 250 ms: {} bytes in, in sequence; {} bytes out, all received in sequence",
        heard.len(),
        written
    );
    // Events and ACL still flow with the voice rings running.
    radio.write_acl(&acl_packet(70)).expect("write_acl");
    assert_eq!(read_acl_bytes(&radio, 70).0, acl_packet(70));

    // ---- closed with the link still up, and opened again ----------------
    drop(radio);
    let again = AokieHciTransport::open(&path).expect("open again");
    again.reset().expect("HCI Reset after reopening");
    assert_eq!(again.read_bd_addr().expect("Read BD_ADDR"), STAND_IN);
    let closed = counters(&again);
    assert_eq!(closed.voice_alt, 0, "the close left the voice interface up");
}

/// The dongle is pulled out while every pipe has transfers with libusb:
/// the reads must end with an error the runtime does not take for
/// "nothing yet", and closing the transport must not hang on transfers
/// that can no longer come back the usual way.
///
/// `AOKIE_VIRTUAL_DONGLE_DETACH` is `<vhci-hcd's detach file>=<port>`:
/// writing the port there is the pull. The dongle is gone afterwards, so
/// this runs last (`run.sh` does it after everything else).
#[test]
#[ignore = "needs the stand-in dongle: scripts/virtual-dongle/run.sh"]
fn pulling_the_stand_in_out_ends_reads_with_an_error_not_a_hang() {
    let path = std::env::var("AOKIE_VIRTUAL_DONGLE")
        .expect("AOKIE_VIRTUAL_DONGLE names the stand-in (usb:<bus>:<address>)");
    let detach = std::env::var("AOKIE_VIRTUAL_DONGLE_DETACH")
        .expect("AOKIE_VIRTUAL_DONGLE_DETACH is <detach file>=<port>");
    let (detach_file, port) = detach.split_once('=').expect("<detach file>=<port>");

    let mut radio = AokieHciTransport::open(&path).expect("open");
    radio.reset().expect("HCI Reset");
    assert_eq!(radio.read_bd_addr().expect("Read BD_ADDR"), STAND_IN);
    radio.set_read_timeouts(5, Some(5), Some(5)).expect("timeouts");
    radio
        .configure_sco_alt_setting(0x0060, 1)
        .expect("configure CVSD");
    // Every pipe gets its transfers out: the event and ACL reads queued,
    // the inbound voice ring given to libusb, a few packets in the
    // outbound one.
    assert!(radio.read_event().is_err(), "nothing was due on the event pipe");
    assert!(radio.read_acl(1024).is_err(), "nothing was due on the ACL pipe");
    let _ = radio.read_sco(255).expect("read_sco");
    for _ in 0..4 {
        radio.write_sco(&[0u8; 51]).expect("write_sco");
    }

    std::fs::write(detach_file, port).expect("pull the stand-in out");

    let gone = |what: &str, mut read: Box<dyn FnMut() -> Result<Vec<u8>, String> + '_>| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < deadline,
                "{} went on as if the dongle were still there",
                what
            );
            match read() {
                Err(e) if e.contains("libusb timeout") => {}
                Err(e) => return e,
                Ok(_) => {}
            }
        }
    };
    let event_error = gone("the event pipe", Box::new(|| radio.read_event()));
    let acl_error = gone("the ACL pipe", Box::new(|| radio.read_acl(1024)));
    println!("after the pull: events: {event_error}\nafter the pull: ACL: {acl_error}");
    let voice_error = gone("the voice pipe", Box::new(|| radio.read_sco(255)));
    println!("after the pull: voice: {voice_error}");
    assert!(radio.write_acl(&acl_packet(20)).is_err());

    let closing = Instant::now();
    drop(radio);
    assert!(
        closing.elapsed() < Duration::from_secs(8),
        "closing a transport whose dongle is gone took {:?}",
        closing.elapsed()
    );
}
