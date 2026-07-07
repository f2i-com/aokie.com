//! Linux Bluetooth-dongle transport — libusb / rusb backend.
//!
//! Real device enumeration, kernel-driver detach, HCI command/event
//! I/O over interrupt-IN, ACL data over bulk-IN/OUT, and SCO audio
//! over isochronous endpoints (live in alt-setting 1). The portable
//! HCI/L2CAP/RFCOMM/HFP/MAP/PBAP stack reaches the dongle through
//! `transport.rs` which fans out to either this file or the WinUSB
//! backend depending on target.
//!
//! ### USB Bluetooth class layout (Bluetooth Core spec, Vol 4 Part B §2.1.1)
//!
//! | Endpoint     | Address | Purpose                                       |
//! |--------------|---------|-----------------------------------------------|
//! | Control      | 0x00    | HCI commands (host → controller)              |
//! | Interrupt-IN | 0x81    | HCI events (controller → host)                |
//! | Bulk-OUT     | 0x02    | ACL data (host → controller)                  |
//! | Bulk-IN      | 0x82    | ACL data (controller → host)                  |
//! | Isoch-OUT    | 0x03    | SCO audio (host → controller, alt > 0)        |
//! | Isoch-IN     | 0x83    | SCO audio (controller → host, alt > 0)        |
//!
//! HCI command via control transfer:
//!   bmRequestType = 0x20  (host-to-device, class, interface)
//!   bRequest      = 0x00
//!   wValue        = 0x0000
//!   wIndex        = interface-number (always 0 for HCI)
//!   data          = HCI command (no packet-type prefix)
//!
//! HCI event via interrupt-IN: each transfer delivers one complete
//! event packet (event_code u8, param_len u8, params...). No
//! cross-packet accumulation needed.
//!
//! ### Kernel-driver detach
//!
//! On a vanilla Linux desktop, `btusb` claims interface 0 of every
//! recognized Bluetooth dongle at plug-time. We have to detach it
//! before we can claim. We track whether `btusb` was originally
//! attached so the Drop impl can put it back when Aokie shuts
//! down — without that, the dongle would stay orphaned until the
//! user replugs.
//!
//! ### Device-path format
//!
//! `usb:<bus>:<address>` (e.g., `usb:1:5`). bus and address are
//! libusb's u8 identifiers. Both are stable until the dongle is
//! physically replugged. The Pairing UI will hand whichever path
//! the user picks back to `open(path)` verbatim.

use std::collections::VecDeque;
use std::os::raw::{c_int, c_uint};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rusb::{Direction, GlobalContext, TransferType, UsbContext};

use crate::aokie_radio::hci;

// libusb-1.0 transfer-status constants we actually care about. The
// libusb1-sys crate exposes these but only as plain `c_int`; we
// re-declare them with named consts so the call sites read.
const LIBUSB_TRANSFER_COMPLETED: c_int = 0;
const LIBUSB_TRANSFER_TYPE_ISOCHRONOUS: u8 = 1;

/// SCO endpoints come alive only after `set_alternate_setting(>=1)`.
/// Alt 1 covers single CVSD/mSBC links (24-byte SCO MPS at 8 kHz; the
/// Bluetooth Core spec table 4.1.5 reserves higher alts for multi-link
/// or 16 kHz cases we don't ship today). The runtime only opens one
/// SCO link at a time, so hardcoding alt 1 covers every voice path
/// the receptionist actually runs.
const SCO_ACTIVE_ALT_SETTING: u8 = 1;
const SCO_INACTIVE_ALT_SETTING: u8 = 0;

/// Iso transfers per SCO read/write call. The single-transfer model
/// keeps the FFI surface minimal — Phase L4b can promote this to a
/// 4-deep ring once we have a Linux box to validate end-to-end audio
/// quality.
const SCO_ISO_PACKETS_PER_TRANSFER: u16 = 4;

/// USB device class for Wireless Controller — the parent class for
/// Bluetooth dongles on the standard interface layout. Subclass 0x01
/// + protocol 0x01 narrows it to "Bluetooth Programming Interface"
/// per the USB-IF Wireless Controller Subclass Bluetooth spec.
const USB_CLASS_WIRELESS: u8 = 0xE0;
const USB_SUBCLASS_BLUETOOTH: u8 = 0x01;
const USB_PROTOCOL_BLUETOOTH: u8 = 0x01;

/// Bluetooth control endpoint requestType: host→device, class,
/// interface-recipient.
const BT_HCI_CMD_REQUEST_TYPE: u8 = 0x20;
const BT_HCI_CMD_REQUEST: u8 = 0x00;

/// HCI events fit comfortably in 260 bytes (max 255 params + 2-byte
/// header + slack). Same buffer size the WinUSB path uses.
const HCI_EVENT_BUFFER: usize = 260;

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
// AokieHciTransport — owns a libusb DeviceHandle and the deferred-event queue.
// =============================================================================

pub struct AokieHciTransport {
    handle: rusb::DeviceHandle<GlobalContext>,
    interface_number: u8,
    /// Whether the kernel had `btusb` (or another driver) bound to
    /// the interface at open time. We detached it so we could claim
    /// the interface; on Drop we reattach so system Bluetooth comes
    /// back without a replug.
    kernel_was_attached: bool,
    /// Cached endpoint descriptors for the active alt-setting so reads
    /// /writes don't re-walk the descriptor tree on every call.
    pipes: HciPipes,
    /// Per-pipe read timeouts (ms). Defaults match the WinUSB path:
    /// 5 ms for the event/acl/sco endpoints. `set_read_timeouts`
    /// updates them.
    timeouts: ReadTimeouts,
    /// Events received while waiting for a Command Complete that
    /// didn't match the expected opcode. `read_event` drains these
    /// before pulling fresh from the wire so async events that
    /// race a command don't get lost.
    deferred_events: Mutex<VecDeque<Vec<u8>>>,
    /// Serializes write_command → read_command_complete pairs so two
    /// concurrent callers can't have their replies tangled.
    command_lock: Mutex<()>,
    /// Most recent SCO config — None until configure_sco_alt_setting
    /// runs (Phase L4).
    sco_transport_config: Option<ScoTransportConfig>,
    /// Hex-dump every transfer when AOKIE_RADIO_DUMP=1.
    dump: PacketDump,
}

#[derive(Debug)]
struct ReadTimeouts {
    event_ms: AtomicU32,
    acl_ms: AtomicU32,
    sco_ms: AtomicU32,
}

impl Default for ReadTimeouts {
    fn default() -> Self {
        Self {
            event_ms: AtomicU32::new(1000),
            acl_ms: AtomicU32::new(1000),
            sco_ms: AtomicU32::new(1000),
        }
    }
}

impl ReadTimeouts {
    #[inline]
    fn event_ms(&self) -> u32 {
        self.event_ms.load(Ordering::Relaxed)
    }
    #[inline]
    fn acl_ms(&self) -> u32 {
        self.acl_ms.load(Ordering::Relaxed)
    }
    #[inline]
    fn sco_ms(&self) -> u32 {
        self.sco_ms.load(Ordering::Relaxed)
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
    pub fn open_first() -> Result<Option<Self>, String> {
        let interfaces = enumerate_hci_radio_interfaces()?;
        match interfaces.into_iter().next() {
            Some(iface) => Ok(Some(Self::open(&iface.path)?)),
            None => Ok(None),
        }
    }

    pub fn open(path: &str) -> Result<Self, String> {
        let (bus, address) = parse_path(path)?;
        let device = find_device(bus, address)?;
        // rusb 0.9 publishes `claim_interface` / `detach_kernel_driver`
        // / `set_alternate_setting` as `&self` methods (libusb is
        // internally thread-safe, so the wrapper doesn't need to take
        // exclusive access). Plain `let` is enough — `mut` would draw
        // an `unused_mut` warning on the Linux build.
        let handle = device
            .open()
            .map_err(|e| format!("libusb open {}: {}", path, fmt_err(e)))?;

        // Walk descriptors once to find the HCI interface (class
        // 0xE0/0x01/0x01) and pre-classify its endpoints. Most
        // dongles put HCI at interface 0; treat that as the default
        // but verify the class bytes match.
        let config = device
            .config_descriptor(0)
            .map_err(|e| format!("libusb config_descriptor: {}", fmt_err(e)))?;
        let (interface_number, alt_setting, pipes) = locate_hci_interface(&config)
            .ok_or_else(|| format!("no HCI interface on USB Bluetooth class device {}", path))?;

        // Detach btusb (or whoever's holding the interface) so we can
        // claim it. kernel_driver_active is Linux-only — on other
        // platforms it returns Err which we map to "no driver attached".
        let kernel_was_attached = handle
            .kernel_driver_active(interface_number)
            .unwrap_or(false);
        if kernel_was_attached {
            handle
                .detach_kernel_driver(interface_number)
                .map_err(|e| {
                    format!(
                        "libusb detach_kernel_driver({}): {} — system Bluetooth (BlueZ) is using the dongle. \
                         Stop bluetoothd or install the udev rule (see Linux port docs).",
                        interface_number,
                        fmt_err(e)
                    )
                })?;
        }

        handle.claim_interface(interface_number).map_err(|e| {
            format!(
                "libusb claim_interface({}): {}",
                interface_number,
                fmt_err(e)
            )
        })?;

        let pipes = classify_hci_pipes(&pipes);
        let _ = alt_setting; // alt 0 is the default; SCO alt-setting changes happen in Phase L4.

        Ok(Self {
            handle,
            interface_number,
            kernel_was_attached,
            pipes,
            timeouts: ReadTimeouts::default(),
            deferred_events: Mutex::new(VecDeque::new()),
            command_lock: Mutex::new(()),
            sco_transport_config: None,
            dump: PacketDump::from_env(),
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

    /// Linux libusb stub. The WinUSB path uses this to drop kernel-
    /// buffered URBs from a previous session; libusb's `claim_interface`
    /// already issues an implicit reset, so this is a no-op on Linux.
    pub fn flush_in_pipes(&self) -> Result<(), String> {
        Ok(())
    }

    /// Linux libusb stub. The WinUSB counterpart drops bulk-IN bytes
    /// queued for a now-dead ACL handle so they don't prepend onto
    /// the next ACL session's first read. libusb-on-Linux doesn't
    /// have the same kernel-buffer behaviour at handle granularity,
    /// so this is a no-op.
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
                Duration::from_millis(self.timeouts.event_ms().max(100) as u64),
            )
            .map_err(|e| format!("libusb write_control (HCI cmd): {}", fmt_err(e)))?;
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
        self.read_event_from_wire()
    }

    fn read_event_from_wire(&self) -> Result<Vec<u8>, String> {
        let event_in = self
            .pipes
            .event_in
            .ok_or_else(|| "libusb interface has no HCI event endpoint".to_string())?;
        let mut buf = vec![0u8; HCI_EVENT_BUFFER];
        let read = self
            .handle
            .read_interrupt(
                event_in.id,
                &mut buf,
                Duration::from_millis(self.timeouts.event_ms() as u64),
            )
            .map_err(|e| format!("libusb read_interrupt (HCI event): {}", fmt_err(e)))?;
        buf.truncate(read);
        self.dump.log("evt <", &buf);
        Ok(buf)
    }

    pub fn set_read_timeouts(
        &self,
        event_timeout_ms: u32,
        acl_timeout_ms: Option<u32>,
        sco_timeout_ms: Option<u32>,
    ) -> Result<(), String> {
        // The runtime calls this with single-digit millisecond values
        // for SCO so the audio loop can pump packets at real-time
        // cadence — leaving the 1000ms defaults in place would stall
        // every iteration on a missing-frame timeout. Atomic stores
        // are enough: timeouts are read once per transfer (microsecond
        // hot path) and rarely written.
        self.timeouts
            .event_ms
            .store(event_timeout_ms, Ordering::Relaxed);
        if let Some(ms) = acl_timeout_ms {
            self.timeouts.acl_ms.store(ms, Ordering::Relaxed);
        }
        if let Some(ms) = sco_timeout_ms {
            self.timeouts.sco_ms.store(ms, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn configure_sco_alt_setting(
        &mut self,
        voice_setting: u16,
        connection_count: usize,
    ) -> Result<(), String> {
        let _ = (voice_setting, connection_count); // observability only
                                                   // Switch the HCI interface to alt 1 so the SCO endpoints
                                                   // become live (max-packet-size flips from 0 → 24 bytes for
                                                   // single-link CVSD on most chipsets, 60 bytes for mSBC).
        self.handle
            .set_alternate_setting(self.interface_number, SCO_ACTIVE_ALT_SETTING)
            .map_err(|e| {
                format!(
                    "libusb set_alternate_setting({}, {}): {}",
                    self.interface_number,
                    SCO_ACTIVE_ALT_SETTING,
                    fmt_err(e)
                )
            })?;

        // Re-walk the descriptor for the new alt-setting so we pick up
        // the SCO endpoint MPS we just unlocked.
        let device = self.handle.device();
        let config = device
            .config_descriptor(0)
            .map_err(|e| format!("libusb config_descriptor (post alt-set): {}", fmt_err(e)))?;
        let new_pipes = locate_alt_pipes(&config, self.interface_number, SCO_ACTIVE_ALT_SETTING)
            .ok_or_else(|| {
                format!(
                    "no alt-setting {} on interface {} after switch",
                    SCO_ACTIVE_ALT_SETTING, self.interface_number
                )
            })?;
        let classified = classify_hci_pipes(&new_pipes);
        // The event/ACL endpoints typically don't change between alt
        // settings, but be defensive and merge — keep alt-0 values for
        // anything alt-1 doesn't redeclare.
        if let Some(p) = classified.sco_in {
            self.pipes.sco_in = Some(p);
        }
        if let Some(p) = classified.sco_out {
            self.pipes.sco_out = Some(p);
        }

        let mps = self.pipes.sco_in.map(|p| p.max_packet_size);
        self.sco_transport_config = Some(ScoTransportConfig {
            alternate_setting: SCO_ACTIVE_ALT_SETTING,
            in_pipe_id: self.pipes.sco_in.map(|p| p.id),
            out_pipe_id: self.pipes.sco_out.map(|p| p.id),
            max_packet_size: mps,
            isoch_buffer_len: mps.map(|m| m as usize * SCO_ISO_PACKETS_PER_TRANSFER as usize),
            isoch_buffers_registered: true,
        });
        Ok(())
    }

    pub fn disable_sco_alt_setting(&mut self) -> Result<(), String> {
        // Match WinUSB's idempotent contract: the runtime calls this
        // on every SCO disconnect even when no SCO link was ever up.
        // If the alt-setting was never raised, set_alternate_setting
        // back to 0 is still safe — libusb just no-ops if the device
        // is already at the requested alt.
        let _ = self
            .handle
            .set_alternate_setting(self.interface_number, SCO_INACTIVE_ALT_SETTING);
        self.sco_transport_config = None;
        Ok(())
    }

    pub fn sco_transport_config(&self) -> Option<ScoTransportConfig> {
        self.sco_transport_config
    }

    /// Whether the controller exposes a usable wide-band-speech (mSBC)
    /// SCO alt-setting. The Linux libusb backend doesn't enumerate the
    /// alt-setting table the way the WinUSB path does, so we
    /// conservatively report `false` — the HFP layer falls back to
    /// advertising CVSD-only in `AT+BAC`. Override at runtime with
    /// `AOKIE_HFP_CODEC=wbs` if mSBC is wanted on Linux.
    pub fn supports_msbc_alt_setting(&self) -> bool {
        false
    }

    pub fn read_command_complete(&self, opcode: u16) -> Result<Vec<u8>, String> {
        // Read directly from the wire — never from the deferred queue.
        // A deferred event by definition is not the command_complete
        // we're waiting for, otherwise it would have been returned
        // already.
        let mut ignored = 0;
        loop {
            let event = self.read_event_from_wire()?;
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
            .write_bulk(
                acl_out.id,
                packet,
                Duration::from_millis(self.timeouts.acl_ms() as u64),
            )
            .map_err(|e| format!("libusb write_bulk (HCI ACL out): {}", fmt_err(e)))?;
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
        // libusb requires the buffer to be a multiple of the endpoint's
        // max-packet-size for OUT transfers and at-least max-packet-size
        // for IN; clamp `max_len` upward so we never short-buffer a
        // larger inbound packet. The runtime's accumulator handles the
        // "we asked for more than we got" case fine, so the headroom
        // costs nothing.
        let mps = acl_in.max_packet_size as usize;
        let cap = max_len.max(mps);
        let mut buf = vec![0u8; cap];
        let read = self
            .handle
            .read_bulk(
                acl_in.id,
                &mut buf,
                Duration::from_millis(self.timeouts.acl_ms() as u64),
            )
            .map_err(|e| format!("libusb read_bulk (HCI ACL in): {}", fmt_err(e)))?;
        buf.truncate(read);
        self.dump.log("acl <", &buf);
        Ok(buf)
    }

    pub fn write_sco(&mut self, packet: &[u8]) -> Result<(), String> {
        let sco_out = self
            .pipes
            .sco_out
            .ok_or_else(|| "libusb interface has no HCI SCO out endpoint".to_string())?;
        if !matches!(sco_out.kind, PipeKind::Isochronous) {
            // Some chipsets expose SCO as bulk on alt 0 (rare); take the
            // simple path then. The runtime only ever calls write_sco
            // after configure_sco_alt_setting raised the alt, so this
            // branch is mostly defensive.
            let written = self
                .handle
                .write_bulk(
                    sco_out.id,
                    packet,
                    Duration::from_millis(self.timeouts.sco_ms() as u64),
                )
                .map_err(|e| format!("libusb write_bulk (HCI SCO out): {}", fmt_err(e)))?;
            self.dump.log("sco >", packet);
            if written != packet.len() {
                return Err(format!(
                    "libusb write_bulk short transfer (HCI SCO out): wrote {} of {} bytes",
                    written,
                    packet.len()
                ));
            }
            return Ok(());
        }
        self.dump.log("sco >", packet);
        // For OUT, fan the packet across SCO_ISO_PACKETS_PER_TRANSFER iso
        // packets sized to the endpoint MPS. If the HCI SCO packet is
        // smaller than that, the trailing iso packets just carry zero
        // bytes — the controller drops them silently.
        let mps = sco_out.max_packet_size as usize;
        if mps == 0 {
            return Err("libusb SCO out endpoint reports max_packet_size=0 \
                        (alt-setting not raised yet?)"
                .to_string());
        }
        let buf_len = SCO_ISO_PACKETS_PER_TRANSFER as usize * mps;
        let mut buffer = vec![0u8; buf_len];
        let copy_len = packet.len().min(buf_len);
        buffer[..copy_len].copy_from_slice(&packet[..copy_len]);
        // Per-packet length: distribute the packet bytes across the iso
        // descriptors, leaving any trailing descriptors at length 0.
        let mut packet_lengths = [0u32; SCO_ISO_PACKETS_PER_TRANSFER as usize];
        let mut remaining = copy_len;
        for slot in packet_lengths.iter_mut() {
            let n = remaining.min(mps);
            *slot = n as u32;
            remaining -= n;
            if remaining == 0 {
                break;
            }
        }
        unsafe {
            run_iso_transfer(
                &self.handle,
                sco_out.id,
                buffer.as_mut_ptr(),
                buf_len,
                &packet_lengths,
                self.timeouts.sco_ms(),
            )?;
        }
        Ok(())
    }

    pub fn read_sco(&mut self, max_len: usize) -> Result<Vec<u8>, String> {
        let sco_in = self
            .pipes
            .sco_in
            .ok_or_else(|| "libusb interface has no HCI SCO in endpoint".to_string())?;
        if !matches!(sco_in.kind, PipeKind::Isochronous) {
            // Defensive bulk-SCO branch, mirror of write_sco above.
            let mps = sco_in.max_packet_size as usize;
            let cap = max_len.max(mps.max(64));
            let mut buf = vec![0u8; cap];
            let read = self
                .handle
                .read_bulk(
                    sco_in.id,
                    &mut buf,
                    Duration::from_millis(self.timeouts.sco_ms() as u64),
                )
                .map_err(|e| format!("libusb read_bulk (HCI SCO in): {}", fmt_err(e)))?;
            buf.truncate(read);
            self.dump.log("sco <", &buf);
            return Ok(buf);
        }
        let mps = sco_in.max_packet_size as usize;
        if mps == 0 {
            return Err("libusb SCO in endpoint reports max_packet_size=0 \
                        (alt-setting not raised yet?)"
                .to_string());
        }
        let buf_len = SCO_ISO_PACKETS_PER_TRANSFER as usize * mps;
        // We always submit a full-sized buffer so the controller can
        // pack as many frames as it has into a single transfer; we
        // truncate to whatever it actually filled before returning.
        let mut buffer = vec![0u8; buf_len];
        // For IN transfers, set every iso packet length to MPS — that's
        // libusb's signal of "give me up to this much per packet".
        let packet_lengths = [mps as u32; SCO_ISO_PACKETS_PER_TRANSFER as usize];
        let descs = unsafe {
            run_iso_transfer(
                &self.handle,
                sco_in.id,
                buffer.as_mut_ptr(),
                buf_len,
                &packet_lengths,
                self.timeouts.sco_ms(),
            )?
        };
        // Walk the per-packet descriptors and concat actual_length
        // bytes from each successful one. Skip stalled / overflowed
        // packets — runtime treats SCO drops as silent gaps.
        let mut data = Vec::with_capacity(buf_len);
        for (i, desc) in descs.iter().enumerate() {
            if desc.status != LIBUSB_TRANSFER_COMPLETED {
                continue;
            }
            let offset = i * mps;
            let n = desc.actual_length as usize;
            if n == 0 || offset + n > buffer.len() {
                continue;
            }
            data.extend_from_slice(&buffer[offset..offset + n]);
            if data.len() >= max_len {
                data.truncate(max_len);
                break;
            }
        }
        self.dump.log("sco <", &data);
        Ok(data)
    }
}

impl Drop for AokieHciTransport {
    fn drop(&mut self) {
        // Best-effort release. Ignoring errors here is the right call
        // — we're shutting down and nothing useful comes from
        // panicking the runtime thread on a USB cleanup hiccup.
        let _ = self.handle.release_interface(self.interface_number);
        if self.kernel_was_attached {
            // Reattach btusb so system Bluetooth is usable again
            // without a physical replug. If this fails (kernel module
            // unloaded, bus controlled by something else), the user's
            // next BlueZ scan will wake the driver back up; this is
            // strictly an ergonomics call.
            let _ = self.handle.attach_kernel_driver(self.interface_number);
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

pub fn diagnose_interface_path(path: &str) -> Result<InterfaceDiagnostics, String> {
    let (bus, address) = parse_path(path)?;
    let device = find_device(bus, address)?;
    let config = device
        .config_descriptor(0)
        .map_err(|e| format!("libusb config_descriptor: {}", fmt_err(e)))?;
    let (interface_number, alternate_setting, pipes) =
        locate_hci_interface(&config).ok_or_else(|| format!("no HCI interface on {}", path))?;
    let classified = classify_hci_pipes(&pipes);
    Ok(InterfaceDiagnostics {
        device_path: path.to_string(),
        interface_number,
        alternate_setting,
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
    // Devices that publish the class at the device descriptor level —
    // simplest case, single-function dongles.
    if desc.class_code() == USB_CLASS_WIRELESS
        && desc.sub_class_code() == USB_SUBCLASS_BLUETOOTH
        && desc.protocol_code() == USB_PROTOCOL_BLUETOOTH
    {
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
            if descriptor.class_code() == USB_CLASS_WIRELESS
                && descriptor.sub_class_code() == USB_SUBCLASS_BLUETOOTH
                && descriptor.protocol_code() == USB_PROTOCOL_BLUETOOTH
            {
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

/// Walk the configuration's interface descriptors looking for the HCI
/// interface (class 0xE0/0x01/0x01) at alt-setting 0. Returns the
/// interface number, the alt-setting, and the endpoint inventory.
fn locate_hci_interface(config: &rusb::ConfigDescriptor) -> Option<(u8, u8, Vec<PipeInfo>)> {
    for interface in config.interfaces() {
        let mut alt0_match: Option<rusb::InterfaceDescriptor> = None;
        for descriptor in interface.descriptors() {
            let is_hci = descriptor.class_code() == USB_CLASS_WIRELESS
                && descriptor.sub_class_code() == USB_SUBCLASS_BLUETOOTH
                && descriptor.protocol_code() == USB_PROTOCOL_BLUETOOTH;
            if is_hci && descriptor.setting_number() == 0 {
                alt0_match = Some(descriptor);
                break;
            }
        }
        if let Some(descriptor) = alt0_match {
            let interface_number = descriptor.interface_number();
            let alt_setting = descriptor.setting_number();
            let mut pipes = Vec::new();
            for endpoint in descriptor.endpoint_descriptors() {
                pipes.push(endpoint_to_pipe_info(&endpoint, alt_setting));
            }
            return Some((interface_number, alt_setting, pipes));
        }
    }
    None
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
        max_packet_size: endpoint.max_packet_size(),
        interval: endpoint.interval(),
        alternate_setting,
    }
}

/// Walk a specific (interface_number, alt_setting) and return its
/// endpoint inventory. Used when `configure_sco_alt_setting` swaps
/// the HCI interface from alt 0 → alt 1: the SCO endpoints publish
/// their real `max_packet_size` only after the alt-setting flip.
fn locate_alt_pipes(
    config: &rusb::ConfigDescriptor,
    interface_number: u8,
    alternate_setting: u8,
) -> Option<Vec<PipeInfo>> {
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.interface_number() == interface_number
                && descriptor.setting_number() == alternate_setting
            {
                let mut pipes = Vec::new();
                for endpoint in descriptor.endpoint_descriptors() {
                    pipes.push(endpoint_to_pipe_info(&endpoint, alternate_setting));
                }
                return Some(pipes);
            }
        }
    }
    None
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

/// Per-packet status read back from a completed iso transfer. The
/// public type stays plain Rust (no FFI shapes leaked) so the
/// `read_sco` body doesn't have to use `libusb1_sys` types directly.
#[derive(Debug, Clone, Copy)]
struct IsoPacketResult {
    actual_length: u32,
    status: c_int,
}

/// Submit a single iso transfer and block until it completes, fails,
/// or times out. Returns the per-packet completion descriptors so the
/// caller can decide how much of the buffer to keep.
///
/// Safety: `buffer` must point to a valid writable region of at least
/// `buffer_len` bytes that outlives this call. `packet_lengths` declares
/// the per-iso-packet length budget; its length sets `num_iso_packets`.
unsafe fn run_iso_transfer(
    handle: &rusb::DeviceHandle<GlobalContext>,
    endpoint: u8,
    buffer: *mut u8,
    buffer_len: usize,
    packet_lengths: &[u32],
    timeout_ms: u32,
) -> Result<Vec<IsoPacketResult>, String> {
    use libusb1_sys::*;

    let num_iso_packets = packet_lengths.len();
    if num_iso_packets == 0 || num_iso_packets > c_int::MAX as usize {
        return Err(format!("invalid iso packet count: {}", num_iso_packets));
    }

    let xfer = libusb_alloc_transfer(num_iso_packets as c_int);
    let xfer = match NonNull::new(xfer) {
        Some(p) => p,
        None => return Err("libusb_alloc_transfer returned null".to_string()),
    };
    let xfer_ptr = xfer.as_ptr();

    // The completion latch. The callback writes 1; the event-loop
    // driver below polls until it sees that. Using a heap-pinned int
    // (Box::leak'd lifetime) instead of a stack local because libusb's
    // contract permits the callback to fire as soon as we submit.
    let completed_box = Box::new(0_i32);
    let completed_ptr = Box::into_raw(completed_box);

    // Safe to write fields directly — libusb_alloc_transfer zero-inits.
    (*xfer_ptr).dev_handle = handle.as_raw();
    (*xfer_ptr).flags = 0;
    (*xfer_ptr).endpoint = endpoint;
    (*xfer_ptr).transfer_type = LIBUSB_TRANSFER_TYPE_ISOCHRONOUS;
    (*xfer_ptr).timeout = timeout_ms as c_uint;
    (*xfer_ptr).status = 0;
    (*xfer_ptr).length = buffer_len as c_int;
    (*xfer_ptr).actual_length = 0;
    (*xfer_ptr).callback = iso_complete_callback;
    (*xfer_ptr).user_data = completed_ptr as *mut _;
    (*xfer_ptr).buffer = buffer;
    (*xfer_ptr).num_iso_packets = num_iso_packets as c_int;

    // Set per-iso-packet length on each descriptor in the trailing
    // flexible array. `iso_packet_desc.as_mut_ptr()` returns a pointer
    // at the start of the array; offsets index packets.
    let descs_ptr = (*xfer_ptr).iso_packet_desc.as_mut_ptr();
    for (i, &len) in packet_lengths.iter().enumerate() {
        let desc = descs_ptr.add(i);
        (*desc).length = len;
        (*desc).actual_length = 0;
        (*desc).status = 0;
    }

    let submit_rc = libusb_submit_transfer(xfer_ptr);
    if submit_rc != 0 {
        libusb_free_transfer(xfer_ptr);
        let _reclaim = Box::from_raw(completed_ptr);
        return Err(format!(
            "libusb_submit_transfer (iso ep 0x{:02x}): {}",
            endpoint, submit_rc
        ));
    }

    // Drive the libusb event loop until the callback flips the latch.
    // We pass `completed_ptr` so libusb returns early if any other
    // pending transfer signals via the same flag (we only have one
    // in flight, so this just shortens the loop).
    let ctx = handle.context().as_raw();
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms as u64 + 50);
    loop {
        if *completed_ptr != 0 {
            break;
        }
        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 50_000,
        };
        libusb_handle_events_timeout_completed(ctx, &tv, completed_ptr);
        if std::time::Instant::now() > deadline {
            // Cancel the transfer. libusb will fire the callback with
            // status = CANCELLED once it processes the cancellation —
            // we MUST wait for that callback before freeing the
            // transfer struct, because libusb_free_transfer on an
            // active transfer is undefined behavior per the docs.
            //
            // We give the cancel up to 5 s of event-loop draining
            // (the deadline-after-deadline below). If the callback
            // still hasn't fired, the device is likely unplugged or
            // hung — at that point we leak the transfer + completion
            // box rather than risk UB. The leak is bounded (one
            // transfer struct + one i32 per stuck SCO call) and
            // recoverable on app restart.
            let _ = libusb_cancel_transfer(xfer_ptr);
            let cancel_deadline = std::time::Instant::now() + Duration::from_millis(5000);
            while *completed_ptr == 0 && std::time::Instant::now() < cancel_deadline {
                let tv = libc::timeval {
                    tv_sec: 0,
                    tv_usec: 50_000,
                };
                libusb_handle_events_timeout_completed(ctx, &tv, completed_ptr);
            }
            break;
        }
    }

    let signalled = *completed_ptr != 0;
    if !signalled {
        // Leak: see comment above. Logging once gives us a forensic
        // breadcrumb if this ever fires in the field.
        eprintln!(
            "[aokie_radio::libusb] iso transfer ep 0x{:02x} stuck after cancel — leaking \
             to avoid UB. Most likely the dongle was unplugged.",
            endpoint
        );
        return Err("libusb timeout".to_string());
    }

    // Safe to read + free: the callback fired, so libusb is done with
    // the transfer struct.
    let mut results = Vec::with_capacity(num_iso_packets);
    for i in 0..num_iso_packets {
        let desc = descs_ptr.add(i);
        results.push(IsoPacketResult {
            actual_length: (*desc).actual_length,
            status: (*desc).status,
        });
    }
    let xfer_status = (*xfer_ptr).status;

    libusb_free_transfer(xfer_ptr);
    let _reclaim = Box::from_raw(completed_ptr);

    if xfer_status != LIBUSB_TRANSFER_COMPLETED {
        // Per-packet status is still useful even when the whole transfer
        // didn't complete cleanly (cancellation, partial success); return
        // them so the caller can scrape what's there. The status int gets
        // logged at the dump path.
        eprintln!(
            "[aokie_radio::libusb] iso transfer ep 0x{:02x} status={}",
            endpoint, xfer_status
        );
    }
    Ok(results)
}

// Match libusb1-sys's `libusb_transfer_cb_fn` exactly: an `extern "system"`
// function pointer with no `unsafe` qualifier on the type. (On Linux
// x86_64 / aarch64 this lowers to the same SysV ABI as `extern "C"`.)
// The unsafe is moved inside the body where the raw-pointer derefs live.
extern "system" fn iso_complete_callback(transfer: *mut libusb1_sys::libusb_transfer) {
    if transfer.is_null() {
        return;
    }
    unsafe {
        let user_data = (*transfer).user_data as *mut i32;
        if !user_data.is_null() {
            *user_data = 1;
        }
    }
}

// Helper used by the libusb stub methods that haven't been ported
// yet (a few tail entries on the transport surface). The current
// callers were folded into the typed-error path; keep the helper
// available behind allow(dead_code) for the next port pass.
#[allow(dead_code)]
fn not_implemented(method: &str) -> String {
    format!(
        "aokie_radio::libusb::{} — Linux transport not implemented yet \
         (see PLAN.md → Linux port)",
        method
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
            PipeInfo {
                id: 0x81,
                kind: PipeKind::Interrupt,
                direction: PipeDirection::In,
                max_packet_size: 16,
                interval: 1,
                alternate_setting: 0,
            },
            PipeInfo {
                id: 0x82,
                kind: PipeKind::Bulk,
                direction: PipeDirection::In,
                max_packet_size: 64,
                interval: 0,
                alternate_setting: 0,
            },
            PipeInfo {
                id: 0x02,
                kind: PipeKind::Bulk,
                direction: PipeDirection::Out,
                max_packet_size: 64,
                interval: 0,
                alternate_setting: 0,
            },
            PipeInfo {
                id: 0x83,
                kind: PipeKind::Isochronous,
                direction: PipeDirection::In,
                max_packet_size: 17,
                interval: 1,
                alternate_setting: 0,
            },
            PipeInfo {
                id: 0x03,
                kind: PipeKind::Isochronous,
                direction: PipeDirection::Out,
                max_packet_size: 17,
                interval: 1,
                alternate_setting: 0,
            },
        ];
        let pipes_ref = pipes.clone();
        let classified = classify_hci_pipes(&pipes_ref);
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
            PipeInfo {
                id: 0x81,
                kind: PipeKind::Interrupt,
                direction: PipeDirection::In,
                max_packet_size: 16,
                interval: 1,
                alternate_setting: 0,
            },
            PipeInfo {
                id: 0x84,
                kind: PipeKind::Interrupt,
                direction: PipeDirection::In,
                max_packet_size: 8,
                interval: 1,
                alternate_setting: 0,
            },
        ];
        let classified = classify_hci_pipes(&pipes);
        assert_eq!(classified.event_in.unwrap().id, 0x81);
    }
}
