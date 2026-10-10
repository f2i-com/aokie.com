//! Linux and macOS Bluetooth-dongle transport — libusb / rusb backend.
//!
//! Real device enumeration, system-driver detach, HCI command/event
//! I/O, ACL data over bulk-IN/OUT, and SCO audio over isochronous
//! endpoints. The portable HCI/L2CAP/RFCOMM/HFP/MAP/PBAP stack reaches
//! the dongle through `transport.rs` which fans out to either this file
//! or the WinUSB backend depending on target.
//!
//! ### USB Bluetooth class layout (Bluetooth Core spec, Vol 4 Part B §2.1.1)
//!
//! | Interface | Endpoint     | Address | Purpose                            |
//! |-----------|--------------|---------|------------------------------------|
//! | 0         | Control      | 0x00    | HCI commands (host → controller)   |
//! | 0         | Interrupt-IN | 0x81    | HCI events (controller → host)     |
//! | 0         | Bulk-OUT     | 0x02    | ACL data (host → controller)       |
//! | 0         | Bulk-IN      | 0x82    | ACL data (controller → host)       |
//! | 1         | Isoch-OUT    | 0x03    | SCO audio (host → controller)      |
//! | 1         | Isoch-IN     | 0x83    | SCO audio (controller → host)      |
//!
//! The voice endpoints sit on an interface of their own (the second)
//! whose alternate settings size them: setting 0 carries nothing, and a
//! voice link switches that interface, never the first, to the setting
//! its sample size and link count call for (`usb_hci`).
//!
//! HCI command via control transfer:
//!   bmRequestType = 0x20  (host-to-device, class, interface)
//!   bRequest      = 0x00
//!   wValue        = 0x0000
//!   wIndex        = interface-number (always 0 for HCI)
//!   data          = HCI command (no packet-type prefix)
//!
//! ### How the pipes are read
//!
//! Every IN pipe is read through a transfer that stays with libusb across
//! the runtime's polls (`xfer`), never through libusb's blocking calls: a
//! blocking read cancelled by its own timeout loses the USB packets that
//! had already arrived. Events are read one USB packet at a time and put
//! together by their own header (`usb_hci::EventStream`), which is how
//! Linux's `btusb` reads them; ACL comes in whole transfers that the
//! runtime's accumulator cuts into packets; voice rides a ring of
//! isochronous transfers each way.
//!
//! ### System-driver detach
//!
//! On a vanilla Linux desktop, `btusb` claims the dongle's interfaces at
//! plug-time; a Mac's own Bluetooth may do the same. We have to detach
//! it before we can claim. We track which interfaces were detached so the
//! Drop impl can put the driver back when Aokie shuts down — without
//! that, the dongle would stay orphaned until the user replugs. (On macOS
//! libusb may detach a system driver only for a root process; see
//! `SYSTEM_HOLDS_IT`.)
//!
//! ### Device-path format
//!
//! `usb:<bus>:<address>` (e.g., `usb:1:5`). bus and address are
//! libusb's u8 identifiers. Both are stable until the dongle is
//! physically replugged. The Pairing UI will hand whichever path
//! the user picks back to `open(path)` verbatim.

mod xfer;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use libusb1_sys::constants::{LIBUSB_TRANSFER_TYPE_BULK, LIBUSB_TRANSFER_TYPE_INTERRUPT};
use rusb::{Direction, GlobalContext, TransferType};

use crate::aokie_radio::hci;
use crate::aokie_radio::usb_hci::{
    choose_sco_alt, msbc_alt_available, sco_alt_setting_for_voice, EventStream, ScoAltChoice,
    STALE_EVENT,
};
use xfer::{IsoIn, IsoOut, QueuedIn};

/// The alternate setting of the voice interface that carries nothing.
const SCO_INACTIVE_ALT_SETTING: u8 = 0;

/// USB device class for Wireless Controller — the parent class for
/// Bluetooth dongles on the standard interface layout. Subclass 0x01
/// + protocol 0x01 narrows it to "Bluetooth Programming Interface"
/// per the USB-IF Wireless Controller Subclass Bluetooth spec.
const USB_CLASS_WIRELESS: u8 = 0xE0;
const USB_SUBCLASS_BLUETOOTH: u8 = 0x01;
const USB_PROTOCOL_BLUETOOTH: u8 = 0x01;

/// Vendor-specific class: Broadcom's own dongles (the BCM20702A0 sold as
/// `0a5c:21ec` among them) and the Broadcom-based ones other vendors sell
/// give their HCI and voice interfaces 0xFF/0x01/0x01, the standard
/// layout under a vendor class. Those are Bluetooth from these vendors
/// only (the vendors Linux's btusb takes them from); Apple's are a Mac's
/// own controllers and are left out.
const USB_CLASS_VENDOR: u8 = 0xFF;
const BROADCOM_VENDOR_CLASS_VIDS: [u16; 8] = [
    0x0a5c, // Broadcom
    0x0b05, // ASUSTek
    0x050d, // Belkin
    0x13d3, // IMC Networks
    0x413c, // Dell
    0x0930, // Toshiba
    0x0489, // Foxconn / Hon Hai
    0x04ca, // Lite-On
];

/// Is a descriptor of this class, from this vendor, a Bluetooth one?
fn is_bluetooth_class(vid: u16, class: u8, sub_class: u8, protocol: u8) -> bool {
    sub_class == USB_SUBCLASS_BLUETOOTH
        && protocol == USB_PROTOCOL_BLUETOOTH
        && (class == USB_CLASS_WIRELESS
            || (class == USB_CLASS_VENDOR && BROADCOM_VENDOR_CLASS_VIDS.contains(&vid)))
}

/// Bluetooth control endpoint requestType: host→device, class,
/// interface-recipient.
const BT_HCI_CMD_REQUEST_TYPE: u8 = 0x20;
const BT_HCI_CMD_REQUEST: u8 = 0x00;

/// How long a write (an HCI command, an ACL packet) may take. Writes are
/// not polls: the runtime's few-millisecond read waits must not cut one
/// short, or half an ACL packet goes out and the link is lost.
const WRITE_TIMEOUT: Duration = Duration::from_millis(1000);

/// The least the queued ACL read asks for: one whole ACL packet of the
/// largest size a classic controller hands over (1021 bytes and a 4-byte
/// header), so that a packet is one transfer.
const ACL_READ_BYTES: usize = 1028;

/// Bound on how many non-matching events we'll defer while waiting
/// for a Command Complete. Ample for any real init sequence; high
/// enough that a pairing event flood can't make a long-running
/// command appear to fail.
const MAX_DEFERRED_EVENTS: usize = 4096;

// =============================================================================
// Public types — shape-compatible with super::winusb so transport::* works.
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioInterface {
    pub path: String,
    pub source: InterfaceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceSource {
    /// Reserved for parity with the Windows enum. Linux doesn't have a
    /// "WinUSB" concept; every dongle we open goes through libusb.
    AokieWinUsb,
    GenericUsbDevice,
    /// Reserved for parity with the Windows enum. Realtek-specific
    /// libusb quirks (firmware patch upload) will live elsewhere; this
    /// variant exists so the diag-CLI's match arms compile on Linux.
    RealtekWinUsb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeKind {
    Bulk,
    Interrupt,
    Isochronous,
    Control,
    Unknown(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeDirection {
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeInfo {
    pub id: u8,
    pub kind: PipeKind,
    pub direction: PipeDirection,
    pub max_packet_size: u16,
    pub interval: u8,
    pub alternate_setting: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HciPipes {
    pub event_in: Option<PipeInfo>,
    pub acl_in: Option<PipeInfo>,
    pub acl_out: Option<PipeInfo>,
    pub sco_in: Option<PipeInfo>,
    pub sco_out: Option<PipeInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDiagnostics {
    pub device_path: String,
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub pipes: Vec<PipeInfo>,
    pub classified: HciPipes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioAddress {
    pub device_path: String,
    pub local_address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerProbe {
    pub device_path: String,
    pub local_address: String,
    pub version: hci::LocalVersion,
    pub features: [u8; 8],
    pub buffer_size: hci::BufferSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoTransportConfig {
    pub alternate_setting: u8,
    pub in_pipe_id: Option<u8>,
    pub out_pipe_id: Option<u8>,
    pub max_packet_size: Option<u16>,
    pub isoch_buffer_len: Option<usize>,
    pub isoch_buffers_registered: bool,
}

// =============================================================================
// AokieHciTransport — owns a libusb DeviceHandle and the transfers on it.
// =============================================================================

/// The two voice pipes while a voice link is up.
struct VoiceStreams {
    input: IsoIn,
    output: IsoOut,
}

pub struct AokieHciTransport {
    interface_number: u8,
    /// The interface that carries the voice endpoints and what each of
    /// its alternate settings offers; `None` when the dongle has none or
    /// it could not be claimed (calls then carry no audio).
    voice: Option<VoiceInterface>,
    /// Interfaces the system's own driver (`btusb`, or macOS's Bluetooth)
    /// was detached from at open. On Drop it is attached again so system
    /// Bluetooth comes back without a replug.
    detached: Vec<u8>,
    /// Cached endpoint descriptors for the active alt-setting so reads
    /// /writes don't re-walk the descriptor tree on every call.
    pipes: HciPipes,
    /// How long `read_event` / `read_acl` wait on their queued read
    /// before reporting a timeout (`set_read_timeouts`). A wait that runs
    /// out cancels nothing.
    timeouts: ReadTimeouts,
    /// Events received while waiting for a Command Complete that
    /// didn't match the expected opcode. `read_event` drains these
    /// before pulling fresh from the wire so async events that
    /// race a command don't get lost.
    deferred_events: Mutex<VecDeque<Vec<u8>>>,
    /// Serializes write_command → read_command_complete pairs so two
    /// concurrent callers can't have their replies tangled.
    command_lock: Mutex<()>,
    /// The reads kept queued on the HCI event and ACL IN pipes: created
    /// on first use, never cancelled by a poll that finds nothing, only
    /// by a pipe flush or by drop.
    event_reader: Mutex<Option<QueuedIn>>,
    acl_reader: Mutex<Option<QueuedIn>>,
    /// Bytes off the event pipe that are not a whole event yet.
    event_stream: Mutex<EventStream>,
    /// The voice pipes' rings, while a voice link is up.
    voice_streams: Option<VoiceStreams>,
    /// Most recent SCO config — None until configure_sco_alt_setting
    /// runs.
    sco_transport_config: Option<ScoTransportConfig>,
    /// Hex-dump every transfer when AOKIE_RADIO_DUMP=1.
    dump: PacketDump,
    /// Last on purpose: every transfer above holds this handle's raw
    /// pointer and must be gone before it closes. (`Drop` takes them down
    /// by hand as well; the order is the second lock on the same door.)
    handle: rusb::DeviceHandle<GlobalContext>,
}

#[derive(Debug)]
struct ReadTimeouts {
    event_ms: AtomicU32,
    acl_ms: AtomicU32,
}

impl Default for ReadTimeouts {
    fn default() -> Self {
        // The WinUSB transport's defaults: a controller may take seconds
        // over its first commands.
        Self {
            event_ms: AtomicU32::new(5000),
            acl_ms: AtomicU32::new(5000),
        }
    }
}

impl ReadTimeouts {
    #[inline]
    fn event(&self) -> Duration {
        Duration::from_millis(self.event_ms.load(Ordering::Relaxed) as u64)
    }
    #[inline]
    fn acl(&self) -> Duration {
        Duration::from_millis(self.acl_ms.load(Ordering::Relaxed) as u64)
    }
}

#[derive(Debug, Clone, Copy)]
struct PacketDump {
    enabled: bool,
}

impl PacketDump {
    fn from_env() -> Self {
        // Same gating as the WinUSB sibling: debug builds honour
        // AOKIE_RADIO_DUMP, release builds refuse it. The dump captures
        // message-body framing in flight, which is too sensitive for a
        // packaged install regardless of how the operator opted in.
        let enabled = cfg!(debug_assertions) && std::env::var_os("AOKIE_RADIO_DUMP").is_some();
        Self { enabled }
    }

    fn log(&self, tag: &str, bytes: &[u8]) {
        if !self.enabled {
            return;
        }
        let preview_len = bytes.len().min(64);
        let mut s = String::with_capacity(preview_len * 3);
        for &b in &bytes[..preview_len] {
            s.push_str(&format!("{:02x} ", b));
        }
        if bytes.len() > preview_len {
            s.push_str(&format!("... ({} total)", bytes.len()));
        }
        eprintln!("[aokie_radio::libusb] {} {}", tag, s.trim_end());
    }
}

impl AokieHciTransport {
    /// The first controller that opens. One that will not open is said
    /// and passed over, as the WinUSB transport does; when none opens and
    /// at least one was there, the last one's reason is the error, so a
    /// dongle the system holds is not reported as "no dongle".
    pub fn open_first() -> Result<Option<Self>, String> {
        let mut last_error = None;
        for interface in enumerate_hci_radio_interfaces()? {
            match Self::open(&interface.path) {
                Ok(transport) => return Ok(Some(transport)),
                Err(e) => {
                    eprintln!(
                        "[AokieRadio] Could not open HCI transport at {}: {}",
                        interface.path, e
                    );
                    last_error = Some(e);
                }
            }
        }
        match last_error {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    pub fn open(path: &str) -> Result<Self, String> {
        let (bus, address) = parse_path(path)?;
        let device = find_device(bus, address)?;
        let handle = device
            .open()
            .map_err(|e| format!("libusb open {}: {}{}", path, fmt_err(e), held_hint(e)))?;

        // Walk the descriptors once: which interface is HCI (commands,
        // events, ACL), which carries voice, and what each of the voice
        // interface's alternate settings offers.
        let config = device
            .config_descriptor(0)
            .map_err(|e| format!("libusb config_descriptor: {}", fmt_err(e)))?;
        let vid = device
            .device_descriptor()
            .map_err(|e| format!("libusb device_descriptor: {}", fmt_err(e)))?
            .vendor_id();
        let layout = plan_layout(&read_descriptors(&config, vid))
            .ok_or_else(|| format!("no HCI interface on USB Bluetooth class device {}", path))?;

        // Detach the system's own Bluetooth driver (btusb on Linux,
        // macOS's host-controller transport on a Mac) so we can claim
        // the interfaces.
        let mut detached = Vec::new();
        if claim(&handle, layout.hci_interface)? {
            detached.push(layout.hci_interface);
        }
        let mut voice = None;
        if let Some(number) = layout.voice_interface {
            if number == layout.hci_interface {
                voice = Some(VoiceInterface {
                    number,
                    alts: layout.voice_alts.clone(),
                });
            } else {
                // Texts and call control need only the first interface, so
                // a voice interface that cannot be had is said, not fatal.
                match claim(&handle, number) {
                    Ok(was_held) => {
                        if was_held {
                            detached.push(number);
                        }
                        voice = Some(VoiceInterface {
                            number,
                            alts: layout.voice_alts.clone(),
                        });
                    }
                    Err(e) => eprintln!(
                        "[AokieRadio] the dongle's voice interface {} could not be claimed, so calls will carry no audio: {}",
                        number, e
                    ),
                }
            }
        }
        if let Some(voice) = voice.as_ref() {
            for alt in &voice.alts {
                eprintln!(
                    "[AokieRadio] SCO alt-setting probe: interface {} alt {} in={} out={}",
                    voice.number,
                    alt.setting,
                    describe_pipe(alt.sco_in),
                    describe_pipe(alt.sco_out),
                );
            }
        }

        Ok(Self {
            interface_number: layout.hci_interface,
            voice,
            detached,
            pipes: classify_hci_pipes(&layout.hci_pipes),
            timeouts: ReadTimeouts::default(),
            deferred_events: Mutex::new(VecDeque::new()),
            command_lock: Mutex::new(()),
            event_reader: Mutex::new(None),
            acl_reader: Mutex::new(None),
            event_stream: Mutex::new(EventStream::default()),
            voice_streams: None,
            sco_transport_config: None,
            dump: PacketDump::from_env(),
            handle,
        })
    }

    pub fn command_complete(&self, command: &[u8], opcode: u16) -> Result<Vec<u8>, String> {
        // Hold the latch across write+read so a second caller can't
        // sneak its command in and have its Command Complete consumed
        // by our reader. Same shape as the WinUSB sibling.
        let _guard = self
            .command_lock
            .lock()
            .map_err(|e| format!("HCI command lock poisoned: {}", e))?;
        self.write_command(command)?;
        self.read_command_complete(opcode)
    }

    pub fn command_return_params(&self, command: &[u8], opcode: u16) -> Result<Vec<u8>, String> {
        let event = self.command_complete(command, opcode)?;
        Ok(hci::parse_command_complete(&event, opcode)?.to_vec())
    }

    pub fn reset(&self) -> Result<(), String> {
        let params = self.command_return_params(&hci::reset_command(), hci::OPCODE_RESET)?;
        hci::expect_status_ok(&params, "HCI Reset")
    }

    /// Start the event and ACL pipes clean. libusb keeps no buffer of its
    /// own between the controller and a transfer, so what there is to
    /// drop is what our own queued reads and the event stream hold from
    /// an earlier use of this transport: take the reads back and forget
    /// the half event. (The HCI Reset that follows empties the
    /// controller's side.)
    pub fn flush_in_pipes(&self) -> Result<(), String> {
        for reader in [&self.event_reader, &self.acl_reader] {
            if let Ok(mut guard) = reader.lock() {
                if let Some(queued) = guard.as_mut() {
                    queued.cancel();
                }
            }
        }
        if let Ok(mut stream) = self.event_stream.lock() {
            stream.clear();
        }
        Ok(())
    }

    /// Nothing to do here. The WinUSB counterpart drops what Windows has
    /// buffered for a dead ACL handle and leaves its queued read alone;
    /// with libusb the controller's bytes go straight into the queued
    /// transfer, which must be left alone for the same reason as there (a
    /// fast-reconnecting phone's first packet may already be in it).
    pub fn flush_acl_in_pipe(&self) -> Result<(), String> {
        Ok(())
    }

    pub fn set_event_mask(&self, mask: u64) -> Result<(), String> {
        let command = hci::set_event_mask_command(mask);
        let params = self.command_return_params(&command, hci::OPCODE_SET_EVENT_MASK)?;
        hci::expect_status_ok(&params, "Set Event Mask")
    }

    pub fn read_local_version(&self) -> Result<hci::LocalVersion, String> {
        let params = self.command_return_params(
            &hci::read_local_version_information_command(),
            hci::OPCODE_READ_LOCAL_VERSION_INFORMATION,
        )?;
        hci::parse_local_version_return(&params)
    }

    pub fn read_local_supported_features(&self) -> Result<[u8; 8], String> {
        let params = self.command_return_params(
            &hci::read_local_supported_features_command(),
            hci::OPCODE_READ_LOCAL_SUPPORTED_FEATURES,
        )?;
        hci::parse_local_supported_features_return(&params)
    }

    pub fn read_buffer_size(&self) -> Result<hci::BufferSize, String> {
        let params = self.command_return_params(
            &hci::read_buffer_size_command(),
            hci::OPCODE_READ_BUFFER_SIZE,
        )?;
        hci::parse_buffer_size_return(&params)
    }

    pub fn read_bd_addr(&self) -> Result<String, String> {
        let params =
            self.command_return_params(&hci::read_bd_addr_command(), hci::OPCODE_READ_BD_ADDR)?;
        hci::parse_read_bd_addr_return(&params)
    }

    pub fn write_command(&self, command: &[u8]) -> Result<(), String> {
        self.dump.log("cmd >", command);
        let written = self
            .handle
            .write_control(
                BT_HCI_CMD_REQUEST_TYPE,
                BT_HCI_CMD_REQUEST,
                0x0000,
                self.interface_number as u16,
                command,
                WRITE_TIMEOUT,
            )
            .map_err(|e| format!("libusb write_control (HCI cmd): {}", fmt_write_err(e)))?;
        if written != command.len() {
            return Err(format!(
                "libusb write_control short transfer: wrote {} of {} bytes",
                written,
                command.len()
            ));
        }
        Ok(())
    }

    pub fn read_event(&self) -> Result<Vec<u8>, String> {
        // Drain any events deferred by `read_command_complete` first —
        // they were already received from the controller, just not the
        // command_complete the caller was looking for.
        if let Ok(mut queue) = self.deferred_events.lock() {
            if let Some(packet) = queue.pop_front() {
                self.dump.log("evt < (deferred)", &packet);
                return Ok(packet);
            }
        }
        let packet = self.read_event_pipe()?;
        self.dump.log("evt <", &packet);
        Ok(packet)
    }

    /// One whole HCI event off the event pipe (never the deferred queue).
    /// Shared by `read_event` and `read_command_complete` so both wait on
    /// the same transfer.
    ///
    /// The pipe is read one USB packet at a time and the event is put
    /// together by its own length. When the wait runs out part-way
    /// through an event, the part that has arrived stays in the stream
    /// and the read stays with libusb; the next call carries on.
    fn read_event_pipe(&self) -> Result<Vec<u8>, String> {
        let event_in = self
            .pipes
            .event_in
            .ok_or_else(|| "libusb interface has no HCI event endpoint".to_string())?;
        let mut stream = self
            .event_stream
            .lock()
            .map_err(|e| format!("HCI event stream lock poisoned: {}", e))?;
        let deadline = Instant::now() + self.timeouts.event();
        loop {
            if let Some(event) = stream.pop() {
                return Ok(event);
            }
            let polled = self.poll_queued(
                &self.event_reader,
                event_in.id,
                LIBUSB_TRANSFER_TYPE_INTERRUPT,
                // Exactly one USB packet: a transfer of that size ends
                // with every packet, full or short, so an event that is a
                // whole number of packets long is not held back until the
                // next event arrives.
                (event_in.max_packet_size as usize).max(1),
                "HCI event",
                deadline.saturating_duration_since(Instant::now()),
            );
            let packet = match polled {
                Ok(packet) => packet,
                Err(e) => {
                    // The read has just come back with nothing, so the rest
                    // of a half event is not waiting in it: only now may the
                    // half be judged stale. (Judged before the read, a caller
                    // who had been away would drop the half it holds while
                    // the rest sat in the queued read, and take the rest for
                    // the start of an event.)
                    if crate::aokie_radio::manager::is_timeout_error(&e) {
                        let dropped = stream.drop_stale(Instant::now(), STALE_EVENT);
                        if dropped > 0 {
                            eprintln!(
                                "[AokieRadio] HCI event stream: {} bytes of an event never got the rest of it; dropped, the next packet starts an event",
                                dropped
                            );
                        }
                    }
                    return Err(e);
                }
            };
            stream.push(&packet);
        }
    }

    /// Poll the read kept queued on one IN pipe, making it on first use.
    fn poll_queued(
        &self,
        reader: &Mutex<Option<QueuedIn>>,
        endpoint: u8,
        kind: u8,
        len: usize,
        label: &'static str,
        wait: Duration,
    ) -> Result<Vec<u8>, String> {
        let mut guard = reader
            .lock()
            .map_err(|e| format!("{} reader lock poisoned: {}", label, e))?;
        if guard.is_none() {
            *guard = Some(QueuedIn::new(&self.handle, endpoint, kind, len, label)?);
        }
        guard.as_mut().expect("reader was just created").poll(wait)
    }

    pub fn set_read_timeouts(
        &self,
        event_timeout_ms: u32,
        acl_timeout_ms: Option<u32>,
        sco_timeout_ms: Option<u32>,
    ) -> Result<(), String> {
        // Event and ACL reads stay queued across waits (`xfer::QueuedIn`),
        // so these are poll waits, not transfer timeouts that would cancel
        // a transfer mid-packet. The voice pipes' rings pace themselves:
        // a read takes what is back and a write waits only for room.
        let _ = sco_timeout_ms;
        self.timeouts
            .event_ms
            .store(event_timeout_ms, Ordering::Relaxed);
        if let Some(ms) = acl_timeout_ms {
            self.timeouts.acl_ms.store(ms, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn configure_sco_alt_setting(
        &mut self,
        voice_setting: u16,
        connection_count: usize,
    ) -> Result<(), String> {
        let Some(asked) = sco_alt_setting_for_voice(voice_setting, connection_count) else {
            return Ok(());
        };
        let Some((number, available)) = self
            .voice
            .as_ref()
            .map(|voice| (voice.number, voice.usable_alts()))
        else {
            return Ok(());
        };
        // A dongle that lacks the setting asked for carries the link on
        // the first 16-bit one it has; a missing setting must not end the
        // call (the WinUSB transport's rule, from dongles in the field).
        let alt = match choose_sco_alt(asked, &available) {
            ScoAltChoice::Asked(alt) => alt,
            ScoAltChoice::Instead { asked, alt } => {
                eprintln!(
                    "[AokieRadio] SCO alt {} not exposed by dongle (available={:?}); falling back to alt {}",
                    asked, available, alt,
                );
                alt
            }
            ScoAltChoice::Nothing { asked } => {
                eprintln!(
                    "[AokieRadio] SCO alt {} not exposed and no 16-bit fallback available (probe={:?}); skipping alt-config",
                    asked, available,
                );
                return Ok(());
            }
        };

        // A voice link still set up (no Disconnection Complete came
        // between two calls) is taken down first, so its transfers are
        // back with us before the interface changes under them.
        if self.voice_streams.is_some() {
            self.disable_sco_alt_setting()?;
        }

        self.switch_voice_interface(number, alt)?;
        let pipes = self
            .voice
            .as_ref()
            .and_then(|voice| voice.alt(alt))
            .ok_or_else(|| {
                format!(
                    "no alt-setting {} on interface {} after switch",
                    alt, number
                )
            })?;
        self.pipes.sco_in = pipes.sco_in;
        self.pipes.sco_out = pipes.sco_out;

        let (Some(sco_in), Some(sco_out)) = (pipes.sco_in, pipes.sco_out) else {
            return Err(format!(
                "alt-setting {} of interface {} has no pair of voice endpoints",
                alt, number
            ));
        };
        let streams = IsoIn::new(&self.handle, sco_in.id, sco_in.max_packet_size).and_then(
            |input| {
                Ok(VoiceStreams {
                    input,
                    output: IsoOut::new(&self.handle, sco_out.id, sco_out.max_packet_size)?,
                })
            },
        );
        let streams = match streams {
            Ok(streams) => streams,
            Err(e) => {
                let _ = self.switch_voice_interface(number, SCO_INACTIVE_ALT_SETTING);
                self.pipes.sco_in = None;
                self.pipes.sco_out = None;
                return Err(e);
            }
        };
        self.voice_streams = Some(streams);
        self.sco_transport_config = Some(ScoTransportConfig {
            alternate_setting: alt,
            in_pipe_id: Some(sco_in.id),
            out_pipe_id: Some(sco_out.id),
            max_packet_size: Some(sco_in.max_packet_size),
            isoch_buffer_len: Some(sco_in.max_packet_size as usize * xfer::VOICE_IN_FRAMES),
            isoch_buffers_registered: true,
        });
        Ok(())
    }

    pub fn disable_sco_alt_setting(&mut self) -> Result<(), String> {
        // Match WinUSB's idempotent contract: the runtime calls this
        // on every SCO disconnect even when no SCO link was ever up.
        //
        // The rings come down first: dropping them takes every transfer
        // back from libusb (and leaks one that will not come back rather
        // than free memory libusb may still write).
        self.voice_streams = None;
        if let Some(number) = self.voice.as_ref().map(|voice| voice.number) {
            let _ = self.switch_voice_interface(number, SCO_INACTIVE_ALT_SETTING);
        }
        self.pipes.sco_in = None;
        self.pipes.sco_out = None;
        self.sco_transport_config = None;
        Ok(())
    }

    /// Switch the voice interface to an alternate setting, if the dongle
    /// is still there.
    ///
    /// Every switch goes through here because of the asking first. libusb
    /// 1.0.26 (what Debian 12 ships) unlocks the handle's mutex without
    /// having locked it when it is asked to switch an interface of a
    /// device that has gone, and its close then aborts the process on that
    /// mutex: a dongle pulled out during a call would take the whole
    /// program with it. (Found by the stand-in dongle's unplug test under
    /// valgrind; libusb 1.0.27 has it right.) The asking reads the device's
    /// configuration, which libusb answers from what the system still
    /// lists and takes no lock for.
    fn switch_voice_interface(&self, interface: u8, alt: u8) -> Result<(), String> {
        if matches!(
            self.handle.active_configuration(),
            Err(rusb::Error::NoDevice)
        ) {
            return Err(format!(
                "libusb set_alternate_setting({}, {}): the dongle is gone",
                interface, alt
            ));
        }
        self.handle
            .set_alternate_setting(interface, alt)
            .map_err(|e| {
                format!(
                    "libusb set_alternate_setting({}, {}): {}",
                    interface,
                    alt,
                    fmt_err(e)
                )
            })
    }

    pub fn sco_transport_config(&self) -> Option<ScoTransportConfig> {
        self.sco_transport_config
    }

    /// Whether the dongle has an alternate setting wide-band speech
    /// (mSBC) can ride: voice setting 0x0043 (transparent, 8-bit input)
    /// asks for an 8-bit setting, and one connection for setting 1. The
    /// same question the WinUSB transport asks, answered from the
    /// descriptors.
    pub fn supports_msbc_alt_setting(&self) -> bool {
        self.voice
            .as_ref()
            .is_some_and(|voice| msbc_alt_available(&voice.usable_alts()))
    }

    pub fn read_command_complete(&self, opcode: u16) -> Result<Vec<u8>, String> {
        // Read directly from the wire — never from the deferred queue.
        // A deferred event by definition is not the command_complete
        // we're waiting for, otherwise it would have been returned
        // already.
        let mut ignored = 0;
        loop {
            let event = self.read_event_pipe()?;
            self.dump.log("evt <", &event);
            match hci::parse_command_complete(&event, opcode) {
                Ok(_) => return Ok(event),
                Err(_) if ignored < MAX_DEFERRED_EVENTS => {
                    if let Ok(mut queue) = self.deferred_events.lock() {
                        queue.push_back(event);
                    }
                    ignored += 1;
                }
                Err(e) => {
                    return Err(format!(
                        "{} (after deferring {} events while waiting for opcode 0x{:04x})",
                        e, ignored, opcode
                    ));
                }
            }
        }
    }

    pub fn write_acl(&self, packet: &[u8]) -> Result<(), String> {
        let acl_out = self
            .pipes
            .acl_out
            .ok_or_else(|| "libusb interface has no HCI ACL out endpoint".to_string())?;
        self.dump.log("acl >", packet);
        let written = self
            .handle
            .write_bulk(acl_out.id, packet, WRITE_TIMEOUT)
            .map_err(|e| format!("libusb write_bulk (HCI ACL out): {}", fmt_write_err(e)))?;
        if written != packet.len() {
            return Err(format!(
                "libusb write_bulk short transfer (HCI ACL out): wrote {} of {} bytes",
                written,
                packet.len()
            ));
        }
        Ok(())
    }

    pub fn read_acl(&self, max_len: usize) -> Result<Vec<u8>, String> {
        let acl_in = self
            .pipes
            .acl_in
            .ok_or_else(|| "libusb interface has no HCI ACL in endpoint".to_string())?;
        // A whole number of USB packets, and room for a whole ACL packet:
        // a bulk transfer ends at the first short packet, so one transfer
        // is normally one ACL packet. A read already queued keeps the
        // length it was made with; the runtime's accumulator treats the
        // pipe as a byte stream either way.
        let mps = (acl_in.max_packet_size as usize).max(1);
        let len = max_len.max(ACL_READ_BYTES).next_multiple_of(mps);
        let packet = self.poll_queued(
            &self.acl_reader,
            acl_in.id,
            LIBUSB_TRANSFER_TYPE_BULK,
            len,
            "HCI ACL in",
            self.timeouts.acl(),
        )?;
        self.dump.log("acl <", &packet);
        Ok(packet)
    }

    pub fn write_sco(&mut self, packet: &[u8]) -> Result<(), String> {
        let streams = self
            .voice_streams
            .as_mut()
            .ok_or_else(|| "libusb interface has no HCI SCO out endpoint".to_string())?;
        self.dump.log("sco >", packet);
        streams.output.write(packet)
    }

    /// The voice bytes that have arrived since the last call (possibly
    /// none). `max_len` is not a bound here any more than on the WinUSB
    /// side: the ring hands over every transfer that is back, and the
    /// runtime's assembler cuts the stream into HCI SCO packets.
    pub fn read_sco(&mut self, max_len: usize) -> Result<Vec<u8>, String> {
        let _ = max_len;
        let streams = self
            .voice_streams
            .as_mut()
            .ok_or_else(|| "libusb interface has no HCI SCO in endpoint".to_string())?;
        let data = streams.input.read()?;
        self.dump.log("sco <", &data);
        Ok(data)
    }
}

impl Drop for AokieHciTransport {
    fn drop(&mut self) {
        // Every transfer back from libusb before the interfaces are let
        // go and the handle closes.
        self.voice_streams = None;
        for reader in [&self.event_reader, &self.acl_reader] {
            match reader.lock() {
                Ok(mut guard) => *guard = None,
                Err(poisoned) => *poisoned.into_inner() = None,
            }
        }
        // Best-effort release. Ignoring errors here is the right call
        // — we're shutting down and nothing useful comes from
        // panicking the runtime thread on a USB cleanup hiccup.
        if let Some(number) = self.voice.as_ref().map(|voice| voice.number) {
            let _ = self.switch_voice_interface(number, SCO_INACTIVE_ALT_SETTING);
            if number != self.interface_number {
                let _ = self.handle.release_interface(number);
            }
        }
        let _ = self.handle.release_interface(self.interface_number);
        // Reattach the system's driver so system Bluetooth is usable
        // again without a physical replug. If this fails (kernel module
        // unloaded, bus controlled by something else), the user's next
        // BlueZ scan will wake the driver back up; this is strictly an
        // ergonomics call.
        for &interface in &self.detached {
            let _ = self.handle.attach_kernel_driver(interface);
        }
    }
}

// =============================================================================
// Free functions — same signatures as the WinUSB backend.
// =============================================================================

pub fn enumerate_radio_interfaces() -> Result<Vec<RadioInterface>, String> {
    let mut out = Vec::new();
    let devices = rusb::devices().map_err(|e| format!("libusb devices(): {}", fmt_err(e)))?;
    for device in devices.iter() {
        if !device_supports_bluetooth(&device) {
            continue;
        }
        let path = format_path(device.bus_number(), device.address());
        out.push(RadioInterface {
            path,
            source: InterfaceSource::GenericUsbDevice,
        });
    }
    Ok(out)
}

/// A USB Bluetooth controller as the system lists it, for a setup screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbRadioDevice {
    /// What `AokieHciTransport::open` takes: `usb:<bus>:<address>`.
    pub path: String,
    pub vid: u16,
    pub pid: u16,
}

/// The USB Bluetooth controllers plugged in, with the ids a catalog goes
/// by. Read from the descriptors the system already holds: no device is
/// opened, so this is safe to ask while the radio is running on one of
/// them and answers the same whoever holds the dongle. (Windows lists its
/// devices another way, with the driver bound to each; this is the other
/// systems' list, where no driver is installed.)
pub fn list_usb_radio_devices() -> Result<Vec<UsbRadioDevice>, String> {
    let mut out = Vec::new();
    let devices = rusb::devices().map_err(|e| format!("libusb devices(): {}", fmt_err(e)))?;
    for device in devices.iter() {
        if !device_supports_bluetooth(&device) {
            continue;
        }
        let Ok(descriptor) = device.device_descriptor() else {
            continue;
        };
        out.push(UsbRadioDevice {
            path: format_path(device.bus_number(), device.address()),
            vid: descriptor.vendor_id(),
            pid: descriptor.product_id(),
        });
    }
    Ok(out)
}

pub fn enumerate_hci_radio_interfaces() -> Result<Vec<RadioInterface>, String> {
    // Linux doesn't have a parallel "WinUSB-claimed" vs "generic-USB"
    // distinction the way the Windows path does, so the HCI-narrowed
    // and the broad enumeration return the same set.
    enumerate_radio_interfaces()
}

pub fn diagnose_first_available() -> Result<Option<InterfaceDiagnostics>, String> {
    match enumerate_hci_radio_interfaces()?.into_iter().next() {
        Some(iface) => Ok(Some(diagnose_interface_path(&iface.path)?)),
        None => Ok(None),
    }
}

/// The dongle's pipes as its descriptors give them: the HCI interface's,
/// then the voice interface's for every alternate setting (as the WinUSB
/// side reports them). Reads descriptors only: the dongle is not opened,
/// so this answers even while the system's own Bluetooth holds it.
pub fn diagnose_interface_path(path: &str) -> Result<InterfaceDiagnostics, String> {
    let (bus, address) = parse_path(path)?;
    let device = find_device(bus, address)?;
    let config = device
        .config_descriptor(0)
        .map_err(|e| format!("libusb config_descriptor: {}", fmt_err(e)))?;
    let vid = device
        .device_descriptor()
        .map_err(|e| format!("libusb device_descriptor: {}", fmt_err(e)))?
        .vendor_id();
    let layout = plan_layout(&read_descriptors(&config, vid))
        .ok_or_else(|| format!("no HCI interface on {}", path))?;
    let mut pipes = layout.hci_pipes.clone();
    for alt in &layout.voice_alts {
        pipes.extend(alt.sco_in);
        pipes.extend(alt.sco_out);
    }
    let classified = classify_hci_pipes(&pipes);
    Ok(InterfaceDiagnostics {
        device_path: path.to_string(),
        interface_number: layout.hci_interface,
        alternate_setting: 0,
        pipes,
        classified,
    })
}

pub fn read_first_local_address() -> Result<Option<RadioAddress>, String> {
    match enumerate_hci_radio_interfaces()?.into_iter().next() {
        Some(iface) => {
            let local_address = read_local_address(&iface.path)?;
            Ok(Some(RadioAddress {
                device_path: iface.path,
                local_address,
            }))
        }
        None => Ok(None),
    }
}

pub fn probe_first_controller() -> Result<Option<ControllerProbe>, String> {
    match enumerate_hci_radio_interfaces()?.into_iter().next() {
        Some(iface) => Ok(Some(probe_controller(&iface.path)?)),
        None => Ok(None),
    }
}

pub fn probe_controller(path: &str) -> Result<ControllerProbe, String> {
    let transport = AokieHciTransport::open(path)?;
    transport.reset()?;
    let version = transport.read_local_version()?;
    let features = transport.read_local_supported_features()?;
    let buffer_size = transport.read_buffer_size()?;
    let local_address = transport.read_bd_addr()?;
    Ok(ControllerProbe {
        device_path: path.to_string(),
        local_address,
        version,
        features,
        buffer_size,
    })
}

pub fn read_local_address(path: &str) -> Result<String, String> {
    let transport = AokieHciTransport::open(path)?;
    transport.reset()?;
    transport.read_bd_addr()
}

pub fn classify_hci_pipes(pipes: &[PipeInfo]) -> HciPipes {
    let mut out = HciPipes::default();
    for &pipe in pipes {
        match (pipe.kind, pipe.direction) {
            (PipeKind::Interrupt, PipeDirection::In) if out.event_in.is_none() => {
                out.event_in = Some(pipe)
            }
            (PipeKind::Bulk, PipeDirection::In) if out.acl_in.is_none() => out.acl_in = Some(pipe),
            (PipeKind::Bulk, PipeDirection::Out) if out.acl_out.is_none() => {
                out.acl_out = Some(pipe)
            }
            (PipeKind::Isochronous, PipeDirection::In) if out.sco_in.is_none() => {
                out.sco_in = Some(pipe)
            }
            (PipeKind::Isochronous, PipeDirection::Out) if out.sco_out.is_none() => {
                out.sco_out = Some(pipe)
            }
            _ => {}
        }
    }
    out
}

// =============================================================================
// Internals.
// =============================================================================

fn parse_path(path: &str) -> Result<(u8, u8), String> {
    // Format: "usb:<bus>:<address>"
    let rest = path
        .strip_prefix("usb:")
        .ok_or_else(|| format!("path {:?} not in usb:<bus>:<address> form", path))?;
    let mut parts = rest.split(':');
    let bus = parts
        .next()
        .ok_or_else(|| format!("path {:?} missing bus number", path))?;
    let address = parts
        .next()
        .ok_or_else(|| format!("path {:?} missing address", path))?;
    if parts.next().is_some() {
        return Err(format!("path {:?} has extra components", path));
    }
    let bus: u8 = bus
        .parse()
        .map_err(|_| format!("path {:?} bus not a u8", path))?;
    let address: u8 = address
        .parse()
        .map_err(|_| format!("path {:?} address not a u8", path))?;
    Ok((bus, address))
}

fn format_path(bus: u8, address: u8) -> String {
    format!("usb:{}:{}", bus, address)
}

fn device_supports_bluetooth(device: &rusb::Device<GlobalContext>) -> bool {
    let Ok(desc) = device.device_descriptor() else {
        return false;
    };
    let vid = desc.vendor_id();
    // Devices that publish the class at the device descriptor level —
    // simplest case, single-function dongles.
    if is_bluetooth_class(
        vid,
        desc.class_code(),
        desc.sub_class_code(),
        desc.protocol_code(),
    ) {
        return true;
    }
    // Composite devices (most modern dongles, including the BCM20702
    // we test against) report class 0xEF/0x02/0x01 at the device level
    // and put the Bluetooth class on interface 0. Walk the config
    // descriptors and look for a matching interface.
    let Ok(config) = device.config_descriptor(0) else {
        return false;
    };
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            if is_bluetooth_class(
                vid,
                descriptor.class_code(),
                descriptor.sub_class_code(),
                descriptor.protocol_code(),
            ) {
                return true;
            }
        }
    }
    false
}

fn find_device(bus: u8, address: u8) -> Result<rusb::Device<GlobalContext>, String> {
    let devices = rusb::devices().map_err(|e| format!("libusb devices(): {}", fmt_err(e)))?;
    for device in devices.iter() {
        if device.bus_number() == bus && device.address() == address {
            return Ok(device);
        }
    }
    Err(format!("no USB device at bus {}, address {}", bus, address))
}

/// Claim `interface`, first taking it from the system's own driver when
/// one holds it. Says whether a driver was detached (and so is owed back
/// when the transport closes).
fn claim(handle: &rusb::DeviceHandle<GlobalContext>, interface: u8) -> Result<bool, String> {
    // Where libusb cannot tell (an Err), it is taken as "no driver
    // attached" and the claim below says otherwise.
    let was_held = handle.kernel_driver_active(interface).unwrap_or(false);
    if was_held {
        handle.detach_kernel_driver(interface).map_err(|e| {
            format!(
                "libusb detach_kernel_driver({}): {} — {}",
                interface,
                fmt_err(e),
                SYSTEM_HOLDS_IT
            )
        })?;
    }
    if let Err(e) = handle.claim_interface(interface) {
        // A driver we detached goes back: a dongle left with no driver
        // at all would be dead to the system until replugged.
        if was_held {
            let _ = handle.attach_kernel_driver(interface);
        }
        return Err(format!(
            "libusb claim_interface({}): {}{}",
            interface,
            fmt_err(e),
            held_hint(e)
        ));
    }
    Ok(was_held)
}

fn describe_pipe(pipe: Option<PipeInfo>) -> String {
    match pipe {
        Some(pipe) => format!(
            "0x{:02x} mps={} interval={}",
            pipe.id, pipe.max_packet_size, pipe.interval
        ),
        None => "none".to_string(),
    }
}

/// One alternate setting of one interface, as the configuration
/// descriptor gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AltDescriptor {
    interface: u8,
    setting: u8,
    /// Wireless controller / Bluetooth / Bluetooth programming interface.
    bluetooth: bool,
    pipes: Vec<PipeInfo>,
}

/// What one alternate setting of the voice interface offers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VoiceAlt {
    setting: u8,
    sco_in: Option<PipeInfo>,
    sco_out: Option<PipeInfo>,
}

/// The interface that carries the voice endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VoiceInterface {
    number: u8,
    alts: Vec<VoiceAlt>,
}

impl VoiceInterface {
    fn alt(&self, setting: u8) -> Option<&VoiceAlt> {
        self.alts.iter().find(|alt| alt.setting == setting)
    }

    /// The settings a voice link can ride: both endpoints there, and
    /// both carrying bytes (setting 0 has them at size 0, or not at all).
    fn usable_alts(&self) -> Vec<u8> {
        usable_alts(&self.alts)
    }
}

fn usable_alts(alts: &[VoiceAlt]) -> Vec<u8> {
    alts.iter()
        .filter(|alt| {
            matches!(
                (alt.sco_in, alt.sco_out),
                (Some(sco_in), Some(sco_out))
                    if sco_in.max_packet_size > 0 && sco_out.max_packet_size > 0
            )
        })
        .map(|alt| alt.setting)
        .collect()
}

/// Which interface is which on a dongle.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    hci_interface: u8,
    hci_pipes: Vec<PipeInfo>,
    voice_interface: Option<u8>,
    voice_alts: Vec<VoiceAlt>,
}

/// `vid` is the device's vendor: whether a vendor-class interface is
/// Bluetooth depends on it ([`is_bluetooth_class`]).
fn read_descriptors(config: &rusb::ConfigDescriptor, vid: u16) -> Vec<AltDescriptor> {
    let mut out = Vec::new();
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            let setting = descriptor.setting_number();
            out.push(AltDescriptor {
                interface: descriptor.interface_number(),
                setting,
                bluetooth: is_bluetooth_class(
                    vid,
                    descriptor.class_code(),
                    descriptor.sub_class_code(),
                    descriptor.protocol_code(),
                ),
                pipes: descriptor
                    .endpoint_descriptors()
                    .map(|endpoint| endpoint_to_pipe_info(&endpoint, setting))
                    .collect(),
            });
        }
    }
    out
}

/// Find the HCI interface and the voice interface among a
/// configuration's alternate settings.
///
/// HCI is the first Bluetooth-class interface whose setting 0 has the
/// event (interrupt IN) endpoint. Voice is the Bluetooth-class interface
/// with isochronous endpoints in any of its settings: the standard
/// layout's second interface, or the HCI interface itself on a dongle
/// that keeps everything on one. Vendor interfaces (firmware download,
/// DFU) are neither.
fn plan_layout(alts: &[AltDescriptor]) -> Option<Layout> {
    let has = |alt: &AltDescriptor, kind: PipeKind| alt.pipes.iter().any(|pipe| pipe.kind == kind);
    let hci = alts
        .iter()
        .find(|alt| alt.bluetooth && alt.setting == 0 && has(alt, PipeKind::Interrupt))
        .or_else(|| alts.iter().find(|alt| alt.bluetooth && alt.setting == 0))?;
    let carries_voice = |interface: u8| {
        alts.iter().any(|alt| {
            alt.interface == interface && alt.bluetooth && has(alt, PipeKind::Isochronous)
        })
    };
    let mut candidates: Vec<u8> = alts
        .iter()
        .map(|alt| alt.interface)
        .filter(|&interface| interface != hci.interface && carries_voice(interface))
        .collect();
    candidates.dedup();
    let voice_interface = candidates
        .first()
        .copied()
        .or_else(|| carries_voice(hci.interface).then_some(hci.interface));
    let voice_alts = match voice_interface {
        Some(interface) => alts
            .iter()
            .filter(|alt| alt.interface == interface)
            .map(|alt| {
                let classified = classify_hci_pipes(&alt.pipes);
                VoiceAlt {
                    setting: alt.setting,
                    sco_in: classified.sco_in,
                    sco_out: classified.sco_out,
                }
            })
            .collect(),
        None => Vec::new(),
    };
    Some(Layout {
        hci_interface: hci.interface,
        hci_pipes: hci.pipes.clone(),
        voice_interface,
        voice_alts,
    })
}

fn endpoint_to_pipe_info(endpoint: &rusb::EndpointDescriptor, alternate_setting: u8) -> PipeInfo {
    PipeInfo {
        id: endpoint.address(),
        kind: match endpoint.transfer_type() {
            TransferType::Control => PipeKind::Control,
            TransferType::Isochronous => PipeKind::Isochronous,
            TransferType::Bulk => PipeKind::Bulk,
            TransferType::Interrupt => PipeKind::Interrupt,
        },
        direction: match endpoint.direction() {
            Direction::In => PipeDirection::In,
            Direction::Out => PipeDirection::Out,
        },
        // The low 11 bits are the size; the bits above them count extra
        // transactions per microframe on a high-speed endpoint.
        max_packet_size: endpoint.max_packet_size() & 0x07ff,
        interval: endpoint.interval(),
        alternate_setting,
    }
}

/// What to do when the system's own Bluetooth has the dongle and will
/// not give it up. On Linux that is BlueZ through `btusb`; on a Mac it
/// is macOS's Bluetooth, and libusb may take a device from a macOS
/// driver only for a process that runs as root (or carries an
/// entitlement Apple gives to virtualisation apps). The app that hosts
/// Aokie must not be run as root (its keychain and its files would be
/// root's), so the text sends the reader to the page that says how to
/// find out what holds the dongle, not to `sudo`.
#[cfg(target_os = "macos")]
const SYSTEM_HOLDS_IT: &str = "macOS's own Bluetooth is using the dongle, and macOS gives a USB device it is using only to a root process. \
     Aokie can use a dongle that macOS leaves alone: docs/HARDWARE.md (macOS) says how to see which it is.";
#[cfg(not(target_os = "macos"))]
const SYSTEM_HOLDS_IT: &str = "system Bluetooth (BlueZ) is using the dongle. \
     Stop bluetoothd or install the udev rule (see Linux port docs).";

/// [`SYSTEM_HOLDS_IT`] after an error that means "someone else has it"
/// (no access, or busy); nothing after any other error.
fn held_hint(e: rusb::Error) -> String {
    match e {
        rusb::Error::Access | rusb::Error::Busy => format!(" — {}", SYSTEM_HOLDS_IT),
        _ => String::new(),
    }
}

fn fmt_err(e: rusb::Error) -> String {
    // Map libusb's Timeout variant to a string the runtime's
    // `is_timeout_error` recognizes (see manager.rs). Other errors
    // get their natural Display.
    match e {
        rusb::Error::Timeout => "libusb timeout".to_string(),
        other => other.to_string(),
    }
}

/// A write's error. A write that timed out is a failure, not "nothing
/// yet", so its text must not be the one `is_timeout_error` looks for.
fn fmt_write_err(e: rusb::Error) -> String {
    match e {
        rusb::Error::Timeout => format!(
            "the dongle did not take it within {} ms",
            WRITE_TIMEOUT.as_millis()
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe(id: u8, kind: PipeKind, max_packet_size: u16, alternate_setting: u8) -> PipeInfo {
        PipeInfo {
            id,
            kind,
            direction: if id & 0x80 != 0 {
                PipeDirection::In
            } else {
                PipeDirection::Out
            },
            max_packet_size,
            interval: if kind == PipeKind::Bulk { 0 } else { 1 },
            alternate_setting,
        }
    }

    /// The standard layout, as a BCM20702 gives it: HCI on interface 0,
    /// voice on interface 1 in six settings, then two vendor interfaces.
    fn standard_dongle() -> Vec<AltDescriptor> {
        let mut alts = vec![AltDescriptor {
            interface: 0,
            setting: 0,
            bluetooth: true,
            pipes: vec![
                pipe(0x81, PipeKind::Interrupt, 16, 0),
                pipe(0x82, PipeKind::Bulk, 64, 0),
                pipe(0x02, PipeKind::Bulk, 64, 0),
            ],
        }];
        for (setting, size) in [(0u8, 0u16), (1, 9), (2, 17), (3, 25), (4, 33), (5, 49)] {
            alts.push(AltDescriptor {
                interface: 1,
                setting,
                bluetooth: true,
                pipes: vec![
                    pipe(0x83, PipeKind::Isochronous, size, setting),
                    pipe(0x03, PipeKind::Isochronous, size, setting),
                ],
            });
        }
        alts.push(AltDescriptor {
            interface: 2,
            setting: 0,
            bluetooth: false,
            pipes: vec![
                pipe(0x84, PipeKind::Bulk, 32, 0),
                pipe(0x04, PipeKind::Bulk, 32, 0),
            ],
        });
        alts.push(AltDescriptor {
            interface: 3,
            setting: 0,
            bluetooth: false,
            pipes: vec![],
        });
        alts
    }

    #[test]
    fn parse_path_round_trips() {
        let (bus, addr) = parse_path("usb:1:5").unwrap();
        assert_eq!(bus, 1);
        assert_eq!(addr, 5);
        assert_eq!(format_path(bus, addr), "usb:1:5");
    }

    #[test]
    fn parse_path_rejects_malformed() {
        assert!(parse_path("").is_err());
        assert!(parse_path("usb:1").is_err());
        assert!(parse_path("usb:1:5:extra").is_err());
        assert!(parse_path("not-usb:1:5").is_err());
        assert!(parse_path("usb:abc:5").is_err());
        assert!(parse_path("usb:1:zzz").is_err());
        // u8 range
        assert!(parse_path("usb:1:300").is_err());
    }

    #[test]
    fn classify_hci_pipes_picks_first_of_each_kind() {
        let pipes = vec![
            pipe(0x81, PipeKind::Interrupt, 16, 0),
            pipe(0x82, PipeKind::Bulk, 64, 0),
            pipe(0x02, PipeKind::Bulk, 64, 0),
            pipe(0x83, PipeKind::Isochronous, 17, 0),
            pipe(0x03, PipeKind::Isochronous, 17, 0),
        ];
        let classified = classify_hci_pipes(&pipes);
        assert_eq!(classified.event_in.unwrap().id, 0x81);
        assert_eq!(classified.acl_in.unwrap().id, 0x82);
        assert_eq!(classified.acl_out.unwrap().id, 0x02);
        assert_eq!(classified.sco_in.unwrap().id, 0x83);
        assert_eq!(classified.sco_out.unwrap().id, 0x03);
    }

    #[test]
    fn classify_hci_pipes_ignores_extras() {
        // Two interrupt-IN pipes should classify only the first as
        // event_in — keeps deterministic behavior for chips with a
        // wakeup interrupt endpoint alongside the HCI event one.
        let pipes = vec![
            pipe(0x81, PipeKind::Interrupt, 16, 0),
            pipe(0x84, PipeKind::Interrupt, 8, 0),
        ];
        let classified = classify_hci_pipes(&pipes);
        assert_eq!(classified.event_in.unwrap().id, 0x81);
    }

    #[test]
    fn the_voice_endpoints_are_found_on_the_second_interface() {
        let layout = plan_layout(&standard_dongle()).expect("a layout");
        assert_eq!(layout.hci_interface, 0);
        assert_eq!(layout.hci_pipes.len(), 3);
        // The voice interface is 1, not the vendor interface 2 (whose
        // bulk pipes are a firmware channel) and not HCI's own.
        assert_eq!(layout.voice_interface, Some(1));
        assert_eq!(layout.voice_alts.len(), 6);
        // Setting 0 carries nothing, so it is not one a link can ride.
        assert_eq!(usable_alts(&layout.voice_alts), vec![1, 2, 3, 4, 5]);
        let two = layout.voice_alts.iter().find(|alt| alt.setting == 2).unwrap();
        assert_eq!(two.sco_in.unwrap().id, 0x83);
        assert_eq!(two.sco_in.unwrap().max_packet_size, 17);
        assert_eq!(two.sco_out.unwrap().id, 0x03);
    }

    #[test]
    fn a_standard_dongle_carries_both_codecs() {
        let layout = plan_layout(&standard_dongle()).unwrap();
        let usable = usable_alts(&layout.voice_alts);
        // 16-bit CVSD asks for setting 2, transparent mSBC for setting 1.
        assert_eq!(
            choose_sco_alt(sco_alt_setting_for_voice(0x0060, 1).unwrap(), &usable),
            ScoAltChoice::Asked(2)
        );
        assert_eq!(
            choose_sco_alt(sco_alt_setting_for_voice(0x0043, 1).unwrap(), &usable),
            ScoAltChoice::Asked(1)
        );
        assert!(msbc_alt_available(&usable));
    }

    #[test]
    fn a_dongle_with_everything_on_one_interface_is_its_own_voice_interface() {
        let alts = vec![
            AltDescriptor {
                interface: 0,
                setting: 0,
                bluetooth: true,
                pipes: vec![
                    pipe(0x81, PipeKind::Interrupt, 16, 0),
                    pipe(0x82, PipeKind::Bulk, 64, 0),
                    pipe(0x02, PipeKind::Bulk, 64, 0),
                    pipe(0x83, PipeKind::Isochronous, 0, 0),
                    pipe(0x03, PipeKind::Isochronous, 0, 0),
                ],
            },
            AltDescriptor {
                interface: 0,
                setting: 1,
                bluetooth: true,
                pipes: vec![
                    pipe(0x81, PipeKind::Interrupt, 16, 1),
                    pipe(0x82, PipeKind::Bulk, 64, 1),
                    pipe(0x02, PipeKind::Bulk, 64, 1),
                    pipe(0x83, PipeKind::Isochronous, 17, 1),
                    pipe(0x03, PipeKind::Isochronous, 17, 1),
                ],
            },
        ];
        let layout = plan_layout(&alts).unwrap();
        assert_eq!(layout.hci_interface, 0);
        assert_eq!(layout.voice_interface, Some(0));
        assert_eq!(usable_alts(&layout.voice_alts), vec![1]);
    }

    #[test]
    fn a_dongle_with_no_voice_endpoints_still_has_its_hci_interface() {
        let alts = vec![standard_dongle().remove(0)];
        let layout = plan_layout(&alts).unwrap();
        assert_eq!(layout.hci_interface, 0);
        assert_eq!(layout.voice_interface, None);
        assert!(layout.voice_alts.is_empty());
    }

    #[test]
    fn a_broadcom_dongle_under_the_vendor_class_is_bluetooth() {
        // A BCM20702A0 (0a5c:21ec) as a Mac lists it: device and
        // interfaces 0 and 1 at 0xFF/0x01/0x01, its firmware channel at
        // 0xFF/0xFF/0xFF and its DFU interface at 0xFE/0x01/0x01.
        assert!(is_bluetooth_class(0x0a5c, 0xff, 0x01, 0x01));
        assert!(!is_bluetooth_class(0x0a5c, 0xff, 0xff, 0xff));
        assert!(!is_bluetooth_class(0x0a5c, 0xfe, 0x01, 0x01));
        // The standard class is Bluetooth whoever made it ...
        assert!(is_bluetooth_class(0x0bda, 0xe0, 0x01, 0x01));
        // ... and the vendor class only from the vendors that use it so.
        assert!(!is_bluetooth_class(0x0bda, 0xff, 0x01, 0x01));
        assert!(!is_bluetooth_class(0x05ac, 0xff, 0x01, 0x01));
    }

    #[test]
    fn a_device_with_no_bluetooth_interface_has_no_layout() {
        let alts = vec![AltDescriptor {
            interface: 0,
            setting: 0,
            bluetooth: false,
            pipes: vec![pipe(0x81, PipeKind::Interrupt, 8, 0)],
        }];
        assert_eq!(plan_layout(&alts), None);
    }

    #[test]
    fn a_write_that_times_out_is_not_read_as_nothing_yet() {
        use crate::aokie_radio::manager::is_timeout_error;
        // A read that found nothing is "nothing yet" to the runtime ...
        assert!(is_timeout_error(&xfer::nothing_yet("HCI event")));
        assert!(is_timeout_error(&fmt_err(rusb::Error::Timeout)));
        // ... and a write the dongle did not take is a failure.
        assert!(!is_timeout_error(&fmt_write_err(rusb::Error::Timeout)));
    }
}
