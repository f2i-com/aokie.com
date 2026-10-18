//! WinRT RFCOMM transport adapter (`BtChannel`).
//!
//! Phase 2 of `docs/NATIVE_BLUETOOTH_TRANSPORT_PLAN.md` (§5 item 3 — the
//! reused OBEX/MAP/PBAP byte-stream machines from `aokie_radio` sit
//! behind "a tiny `BtChannel` read/write adapter"). This module is that
//! adapter: a blocking byte channel over a WinRT `StreamSocket` to one of
//! the phone's short-UUID services (MAP MAS `0x1132`, PBAP PSE `0x112F`).
//! Windows resolves the service's RFCOMM channel from its own SDP cache,
//! so none of the dongle's SDP-client work is needed here.
//!
//! BLOCKING, BUT BOUNDED: every method waits on a WinRT async step, and
//! WinRT gives a `DataReader` no read deadline of its own, so each wait is
//! capped here (`bounded!`: [`CONNECT_LIMIT`] for finding and connecting
//! the phone's service, [`IO_LIMIT`] for each write and read). A phone that
//! stops answering mid-exchange then fails the op, which the session
//! drivers already handle (the channel is dropped and the next op opens a
//! fresh one), instead of holding the messaging thread for good. Session
//! drivers also bound every op to [`MAX_OBEX_ROUND_TRIPS`] packets. The
//! messaging lane runs on a thread of its own (`worker.rs`), so even a slow
//! op never holds up call control.

use std::time::{Duration, Instant};

use aokie_bluetooth::aokie_radio::obex::{FixedPayload, Packet};
use windows::Devices::Bluetooth::Rfcomm::RfcommServiceId;
use windows::Devices::Bluetooth::{BluetoothDevice, BluetoothError};
use windows::Networking::Sockets::StreamSocket;
use windows::Storage::Streams::{DataReader, DataWriter};

/// Sanity ceiling for one OBEX response packet. We advertise 0x4000 as
/// our max inbound packet at CONNECT (`map_mas::DEFAULT_MAX_PACKET_LENGTH`
/// / `pbap::DEFAULT_MAX_PACKET_LENGTH`); a response header claiming more
/// than 64 KiB is a corrupted stream, not a legitimately large body
/// (bodies span MULTIPLE packets via Body/EndOfBody chunks).
pub(crate) const MAX_OBEX_PACKET_BYTES: usize = 0x1_0000;

/// Hard round-trip bound for one OBEX session op (CONNECT + SETPATH
/// chain + GET continuations / SRM stream). At the negotiated 16 KiB
/// MTU this caps a single op's body at ~4 MiB — far beyond a 2000-card
/// phonebook — while guaranteeing a wedged server (e.g. the Pixel
/// mid-stream stall the dongle's MAP_OP_DEADLINE exists for) surfaces as
/// an error instead of looping the worker thread forever.
pub(crate) const MAX_OBEX_ROUND_TRIPS: u32 = 256;

/// The longest one step of finding and connecting a phone's service may
/// take: Windows pages an idle phone up for it (a few seconds) and may ask it
/// for its services over the air.
const CONNECT_LIMIT: Duration = Duration::from_secs(20);

/// The longest one write or read on an open channel may take. An OBEX
/// response is normally back in well under a second; the dongle gives a
/// whole MAP op 30 s (`MAP_OP_DEADLINE`).
const IO_LIMIT: Duration = Duration::from_secs(15);

/// How often a bounded wait looks at the step's status.
const WAIT_POLL: Duration = Duration::from_millis(5);

/// Wait at most `$limit` for a WinRT async step (an `IAsyncOperation` or an
/// `IAsyncAction`) and take its result, the error naming `$what`. Past the
/// limit the step is cancelled and the wait ends in an error. A macro, not a
/// function: the async interfaces share these methods but no trait this
/// crate can name.
macro_rules! bounded {
    ($op:expr, $limit:expr, $what:expr) => {{
        let op = $op;
        let limit: Duration = $limit;
        let deadline = Instant::now() + limit;
        loop {
            // AsyncStatus::Started is 0; anything else is over, and `get`
            // returns its result (or its error) at once.
            match op.Status() {
                Ok(status) if status.0 == 0 => {}
                _ => break op.get().map_err(|e| format!("{}: {}", $what, hr(&e))),
            }
            if Instant::now() >= deadline {
                let _ = op.Cancel();
                break Err(format!("{}: no answer in {} s", $what, limit.as_secs()));
            }
            std::thread::sleep(WAIT_POLL);
        }
    }};
}

/// One blocking RFCOMM byte channel to a service on the phone.
pub(crate) struct BtChannel {
    /// Never read directly — kept alive because the streams die with the
    /// socket (and dropping the socket is what closes the channel).
    _socket: StreamSocket,
    writer: DataWriter,
    reader: DataReader,
}

impl BtChannel {
    /// Connect to a service on the phone (blocking). `short_uuid` e.g.
    /// 0x1132 (MAP MAS), 0x112F (PBAP PSE). The phone must already be
    /// paired with Windows; pairing is Windows-owned in native mode
    /// (plan §4 "Pairing UX").
    pub(crate) fn connect(phone_address: u64, short_uuid: u16) -> Result<Self, String> {
        ensure_ro_initialized()?;
        let device = bounded!(
            BluetoothDevice::FromBluetoothAddressAsync(phone_address)
                .map_err(|e| format!("BluetoothDevice::FromBluetoothAddressAsync: {}", hr(&e)))?,
            CONNECT_LIMIT,
            format!("no Bluetooth device at address {phone_address:012X}")
        )?;
        let service_id = RfcommServiceId::FromShortId(short_uuid as u32).map_err(|e| {
            format!(
                "RfcommServiceId::FromShortId(0x{short_uuid:04X}): {}",
                hr(&e)
            )
        })?;
        // NOTE: in windows-rs 0.61.3 this takes ONE argument — there is
        // no cache-mode overload in the projection.
        let result = bounded!(
            device
                .GetRfcommServicesForIdAsync(&service_id)
                .map_err(|e| format!("GetRfcommServicesForIdAsync: {}", hr(&e)))?,
            CONNECT_LIMIT,
            "GetRfcommServicesForIdAsync wait"
        )?;
        let status = result
            .Error()
            .map_err(|e| format!("RfcommDeviceServicesResult::Error: {}", hr(&e)))?;
        if status != BluetoothError::Success {
            // DeviceNotConnected(3) = phone off/out of range;
            // ResourceInUse(2) = another app holds the service (Phone
            // Link contention is plan §7); ConsentRequired(8) = the
            // process lacks the bluetooth capability.
            return Err(format!(
                "RFCOMM service 0x{short_uuid:04X} unavailable on {phone_address:012X}: BluetoothError({})",
                status.0
            ));
        }
        let list = result
            .Services()
            .map_err(|e| format!("RfcommDeviceServicesResult::Services: {}", hr(&e)))?;
        if list.Size().map_err(|e| hr(&e))? == 0 {
            return Err(format!(
                "phone {phone_address:012X} advertises no RFCOMM service 0x{short_uuid:04X}"
            ));
        }
        let service = list.GetAt(0).map_err(|e| hr(&e))?;
        let host = service
            .ConnectionHostName()
            .map_err(|e| format!("RfcommDeviceService::ConnectionHostName: {}", hr(&e)))?;
        let port = service
            .ConnectionServiceName()
            .map_err(|e| format!("RfcommDeviceService::ConnectionServiceName: {}", hr(&e)))?;
        let socket = StreamSocket::new().map_err(|e| format!("StreamSocket::new: {}", hr(&e)))?;
        bounded!(
            socket
                .ConnectAsync(&host, &port)
                .map_err(|e| format!("StreamSocket::ConnectAsync: {}", hr(&e)))?,
            CONNECT_LIMIT,
            format!("RFCOMM connect to {phone_address:012X} svc 0x{short_uuid:04X}")
        )?;
        let writer = DataWriter::CreateDataWriter(
            &socket
                .OutputStream()
                .map_err(|e| format!("StreamSocket::OutputStream: {}", hr(&e)))?,
        )
        .map_err(|e| format!("DataWriter::CreateDataWriter: {}", hr(&e)))?;
        let reader = DataReader::CreateDataReader(
            &socket
                .InputStream()
                .map_err(|e| format!("StreamSocket::InputStream: {}", hr(&e)))?,
        )
        .map_err(|e| format!("DataReader::CreateDataReader: {}", hr(&e)))?;
        Ok(Self {
            _socket: socket,
            writer,
            reader,
        })
    }

    pub(crate) fn write_all(&self, bytes: &[u8]) -> Result<(), String> {
        self.writer
            .WriteBytes(bytes)
            .map_err(|e| format!("RFCOMM write: {}", hr(&e)))?;
        let stored = bounded!(
            self.writer
                .StoreAsync()
                .map_err(|e| format!("RFCOMM flush: {}", hr(&e)))?,
            IO_LIMIT,
            "RFCOMM flush wait"
        )?;
        if stored as usize != bytes.len() {
            return Err(format!(
                "RFCOMM short write: {stored} of {} bytes stored",
                bytes.len()
            ));
        }
        Ok(())
    }

    /// Fill `buf` completely. `LoadAsync(n)` returns as soon as AT LEAST
    /// one byte is available (up to n), so this loops until the buffer is
    /// full; a clean channel close (0 bytes) or any OS error aborts.
    pub(crate) fn read_exact(&self, buf: &mut [u8]) -> Result<(), String> {
        let mut off = 0usize;
        while off < buf.len() {
            let want = (buf.len() - off).min(u32::MAX as usize) as u32;
            let loaded = self.load(want)?;
            if loaded == 0 {
                return Err(format!(
                    "RFCOMM channel closed with {} of {} bytes read",
                    off,
                    buf.len()
                ));
            }
            self.reader
                .ReadBytes(&mut buf[off..off + loaded])
                .map_err(|e| format!("RFCOMM read: {}", hr(&e)))?;
            off += loaded;
        }
        Ok(())
    }

    /// One read op: up to `buf.len()` bytes; 0 means the channel closed.
    /// Part of the fixed channel interface; the current session drivers
    /// only need `read_exact` (OBEX is length-prefixed), so this waits
    /// for the MNS-server phase to find its caller.
    #[allow(dead_code)]
    pub(crate) fn read_some(&self, buf: &mut [u8]) -> Result<usize, String> {
        if buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(u32::MAX as usize) as u32;
        let loaded = self.load(want)?;
        if loaded > 0 {
            self.reader
                .ReadBytes(&mut buf[..loaded])
                .map_err(|e| format!("RFCOMM read: {}", hr(&e)))?;
        }
        Ok(loaded)
    }

    fn load(&self, want: u32) -> Result<usize, String> {
        let loaded = bounded!(
            self.reader
                .LoadAsync(want)
                .map_err(|e| format!("RFCOMM load: {}", hr(&e)))?,
            IO_LIMIT,
            "RFCOMM load wait"
        )?;
        Ok(loaded as usize)
    }
}

impl Drop for BtChannel {
    fn drop(&mut self) {
        // Stream-detach hygiene per the WinRT docs: hand the underlying
        // streams back so the DataReader/DataWriter finalizers can't
        // close the socket underneath us twice. Best-effort — teardown
        // must never panic.
        let _ = self.writer.DetachStream();
        let _ = self.reader.DetachStream();
    }
}

/// Read exactly one OBEX response packet off the channel (3-byte header
/// + declared body) and parse it with the expected fixed-payload shape.
/// Shared by the MAP and PBAP session drivers.
pub(crate) fn read_obex_packet(chan: &BtChannel, fixed: FixedPayload) -> Result<Packet, String> {
    let mut head = [0u8; 3];
    chan.read_exact(&mut head)?;
    let total = u16::from_be_bytes([head[1], head[2]]) as usize;
    if !(3..=MAX_OBEX_PACKET_BYTES).contains(&total) {
        return Err(format!(
            "OBEX response header claims {total} bytes — treating the stream as corrupt"
        ));
    }
    let mut body = vec![0u8; total - 3];
    chan.read_exact(&mut body)?;
    let mut full = Vec::with_capacity(total);
    full.extend_from_slice(&head);
    full.extend_from_slice(&body);
    Packet::parse(&full, fixed)
}

/// WinRT needs COM initialized on the calling thread. Idempotent:
/// S_FALSE (1) = already initialized in a compatible apartment;
/// RPC_E_CHANGED_MODE (0x80010106) = the thread is an STA, which is fine
/// for the free-threaded WinRT objects used here. Anything else means
/// COM is genuinely unusable on this thread.
fn ensure_ro_initialized() -> Result<(), String> {
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_TYPE};
    unsafe {
        match RoInitialize(RO_INIT_TYPE(1)) {
            Ok(()) => Ok(()),
            Err(e) if e.code().0 == 1 => Ok(()),
            Err(e) if e.code().0 == -2_147_417_850 => Ok(()),
            Err(e) => Err(format!("RoInitialize: {}", hr(&e))),
        }
    }
}

/// HRESULT + message, matching the probe.rs diagnostic format.
fn hr(e: &windows::core::Error) -> String {
    format!("0x{:08X} {}", e.code().0, e.message())
}
