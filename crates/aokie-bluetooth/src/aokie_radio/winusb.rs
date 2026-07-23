#![cfg(target_os = "windows")]

use std::collections::VecDeque;
use std::mem::{size_of, zeroed};
use std::ptr::NonNull;
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Devices::Usb::{
    UsbdPipeTypeBulk, UsbdPipeTypeInterrupt, UsbdPipeTypeIsochronous, WinUsb_AbortPipe,
    WinUsb_ControlTransfer, WinUsb_FlushPipe, WinUsb_Free, WinUsb_GetAssociatedInterface,
    WinUsb_GetCurrentFrameNumber, WinUsb_GetOverlappedResult, WinUsb_Initialize,
    WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_ReadIsochPipeAsap, WinUsb_ReadPipe,
    WinUsb_RegisterIsochBuffer, WinUsb_ResetPipe, WinUsb_SetCurrentAlternateSetting,
    WinUsb_SetPipePolicy, WinUsb_UnregisterIsochBuffer, WinUsb_WriteIsochPipeAsap,
    WinUsb_WritePipe, PIPE_TRANSFER_TIMEOUT, USBD_ISO_PACKET_DESCRIPTOR, USB_INTERFACE_DESCRIPTOR,
    WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION, WINUSB_SETUP_PACKET,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING,
    ERROR_NO_MORE_ITEMS, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, OVERLAPPED};

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// Process-wide counter for SCO TX `WinUsb_WriteIsochPipeAsap` calls
/// that returned `ERROR_INVALID_PARAMETER` (Win32 87) and forced us to
/// re-arm with `ContinueStream=FALSE`. A handful per call is normal
/// (every TTS-pause boundary leaves the iso ring empty), but a steady
/// climb during active speech points at SCO TX FIFO underruns —
/// audible as a click at every chain death. Surfaced into the
/// runtime status so the UI can show a "TX stream resets" diagnostic
/// alongside dropped RX frames.
static SCO_TX_STREAM_RESETS: AtomicU64 = AtomicU64::new(0);

/// Returns the cumulative TX stream-reset count since process start.
/// Monotonic; safe to call from any thread.
pub fn sco_tx_stream_resets() -> u64 {
    SCO_TX_STREAM_RESETS.load(AtomicOrdering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioInterface {
    pub path: String,
    pub source: InterfaceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceSource {
    AokieWinUsb,
    GenericUsbDevice,
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
    pub version: crate::aokie_radio::hci::LocalVersion,
    pub features: [u8; 8],
    pub buffer_size: crate::aokie_radio::hci::BufferSize,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterfaceSlot {
    Control,
    Associated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PipeEndpoint {
    info: PipeInfo,
    slot: InterfaceSlot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TransportPipeSet {
    event_in: Option<PipeEndpoint>,
    acl_in: Option<PipeEndpoint>,
    acl_out: Option<PipeEndpoint>,
    sco_in: Option<PipeEndpoint>,
    sco_out: Option<PipeEndpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PacketDump {
    enabled: bool,
}

struct WinUsbIsochBuffer {
    handle: NonNull<std::ffi::c_void>,
    storage: Vec<u8>,
}

struct ScoIsochBuffers {
    in_buffer: WinUsbIsochBuffer,
    out_buffer: WinUsbIsochBuffer,

    /// SCO IN: ring of `SCO_IN_RING_SLOTS` reads kept in flight. Each
    /// slot carries `SCO_IN_PACKETS_PER_SLOT` isoch packets and uses
    /// a heap-stable `Box<OVERLAPPED>` so the OS-held pointer stays
    /// valid across function-call boundaries.
    ///
    /// We submit `SCO_IN_RING_SLOTS` reads up front, all with
    /// ContinueStream=FALSE (matches BTstack's `usb_sco_start`), and
    /// re-submit each slot with ContinueStream=TRUE as soon as it
    /// completes. With only one read in flight at a time we hit the
    /// same problem the TX path used to: every gap between completion
    /// and the next submission is data the controller silently drops,
    /// and at 8 kHz × 16-bit that's most of the inbound audio.
    ///
    /// Mirrors BTstack's `usb_submit_sco_in_transfer_asap` /
    /// `usb_process_sco_in` pair (`ISOC_BUFFERS = 8`,
    /// `NUM_ISO_PACKETS = 3`).
    in_slots: Vec<ScoInSlot>,
    in_drain_idx: usize,
    in_pending: usize,
    in_packet_size: u16,

    /// SCO OUT: ring of `SCO_OUT_RING_SLOTS` slots, each carrying a
    /// heap-stable `Box<OVERLAPPED>` (so the OS-held pointer stays
    /// valid across function-call boundaries) + a manual-reset event
    /// for completion. Each slot has its own region in
    /// `out_buffer.storage` at offset `idx * SCO_OUT_SLOT_STRIDE`
    /// (MPS-aligned per `WinUsb_WriteIsochPipeAsap` requirements).
    ///
    /// We submit writes asynchronously and only block on the oldest
    /// slot when the ring fills. This is what BTstack's
    /// `usb_send_sco_packet` / `usb_process_sco_out` pair does, and
    /// it's the only way `ContinueStream=TRUE` works correctly:
    /// `WinUsb_WriteIsochPipeAsap` with ContinueStream=TRUE rejects
    /// the call (Win32 error 87) if no previous transfer is still
    /// queued, so we must keep at least one in flight at all times
    /// to chain audio frames into a continuous stream.
    out_slots: Vec<ScoOutSlot>,
    out_write_idx: usize,
    out_drain_idx: usize,
    out_pending: usize,
    /// Endpoint MaximumPacketSize for SCO OUT, captured at iso buffer
    /// register time. Drives `out_slot_stride` (slot offsets must be
    /// multiples of this value per `WinUsb_WriteIsochPipeAsap` docs).
    /// 17 at alt 2 (CVSD), 63 at alt 6 (mSBC).
    out_packet_size: u16,
    /// Per-slot byte stride in `out_buffer.storage`. Computed as
    /// `align_up(SCO_OUT_MAX_HCI_PACKET_BYTES, out_packet_size)`. At
    /// alt 2 this is 68 (4 × 17); at alt 6 it's 63 (1 × 63). The
    /// transfer Length we hand WinUSB stays the real `packet.len()`
    /// (51 for CVSD, 63 for mSBC) — only the slot offsets respect
    /// MPS alignment.
    out_slot_stride: usize,
}

struct ScoOutSlot {
    /// Boxed so the heap address is stable — WinUSB stores the raw
    /// pointer for the duration of the asynchronous I/O, and any
    /// reallocation of the containing Vec must NOT move this struct.
    overlapped: Box<OVERLAPPED>,
    event: EventHandle,
    in_use: bool,
}

struct ScoInSlot {
    /// Heap-stable so reallocation of the containing `Vec` cannot
    /// move the address WinUSB has captured.
    overlapped: Box<OVERLAPPED>,
    event: EventHandle,
    /// One descriptor per isoch packet — WinUSB writes each frame's
    /// per-frame `Offset` and `Length` here on completion.
    descriptors: Box<[USBD_ISO_PACKET_DESCRIPTOR]>,
    in_use: bool,
}

/// Number of in-flight SCO out transfers we keep queued. BTstack
/// uses 20; 16 keeps the ring small while still giving enough
/// headroom that a single slow loop tick can't drain the queue.
const SCO_OUT_RING_SLOTS: usize = 16;
/// Largest HCI SCO data packet we ever submit (across codecs):
///   - CVSD:  48-byte payload + 3-byte header = 51 bytes (HCI len 0x30).
///   - mSBC:  60-byte payload + 3-byte header = 63 bytes (HCI len 0x3C).
///
/// Used as the upper bound on per-call `WinUsb_WriteIsochPipeAsap`
/// `Length`. The actual `Length` we pass is `packet.len()` (the real
/// HCI packet bytes), not this constant — we only check `<=` here.
const SCO_OUT_MAX_HCI_PACKET_BYTES: usize = 63;
/// `WinUsb_WriteIsochPipeAsap`'s `BufferOffset` argument MUST be a
/// multiple of the endpoint `MaximumPacketSize` (different per alt
/// setting: 17 at alt 2 for CVSD-16-bit, 63 at alt 6 for mSBC). The
/// per-slot stride is therefore computed at iso buffer registration
/// time as `align_up(SCO_OUT_MAX_HCI_PACKET_BYTES, out_mps)` and
/// stored on `ScoIsochBuffers::out_slot_stride`.
fn align_up(value: usize, alignment: usize) -> usize {
    if alignment == 0 {
        value
    } else {
        ((value + alignment - 1) / alignment) * alignment
    }
}
/// In-flight SCO IN reads. BTstack uses 8, but our runtime loop tick
/// is ~30 ms — and a chain of 8 × 3 × 1 ms = 24 ms dies between iters
/// (every slot completes before we drain), making every re-submit
/// fail with `ContinueStream=TRUE` (Win32 87). 16 slots → 48 ms of
/// queued audio, comfortably longer than one iter, so the chain stays
/// alive across drains.
const SCO_IN_RING_SLOTS: usize = 16;
/// Isoch packets bundled into one SCO IN ASAP read. 3 matches BTstack
/// and is the smallest count that lets ContinueStream chaining keep
/// up with the controller's 1 ms-per-frame production rate without a
/// per-packet syscall.
const SCO_IN_PACKETS_PER_SLOT: usize = 3;

const AOKIE_WINUSB_GUID: GUID = GUID::from_u128(0xb6f5f3a8_6e2c_41d8_9b7a_6e35f60480d6);
const GUID_DEVINTERFACE_USB_DEVICE: GUID = GUID::from_u128(0xa5dcbf10_6530_11d2_901f_00c04fb951ed);
const GUID_DEVINTERFACE_WINUSB_REALTEK: GUID =
    GUID::from_u128(0x226e0e8f_afc0_4a68_864d_ef7e9553e1ea);
const SCO_ALT_SETTINGS_TO_PROBE: [u8; 7] = [0, 1, 2, 3, 4, 5, 6];
/// BTstack USB-transport alt-setting tables. Bit 5 of voice_setting
/// (input sample size) selects which table; the connection count
/// indexes into it. mSBC + transparent + 1 connection → alt 1.
const SCO_ALT_SETTINGS_8_BIT: [u8; 3] = [1, 2, 3];
const SCO_ALT_SETTINGS_16_BIT: [u8; 3] = [2, 4, 5];

pub fn enumerate_radio_interfaces() -> Result<Vec<RadioInterface>, String> {
    let mut out = Vec::new();
    for (source, guid) in [
        (InterfaceSource::AokieWinUsb, AOKIE_WINUSB_GUID),
        (
            InterfaceSource::GenericUsbDevice,
            GUID_DEVINTERFACE_USB_DEVICE,
        ),
        (
            InterfaceSource::RealtekWinUsb,
            GUID_DEVINTERFACE_WINUSB_REALTEK,
        ),
    ] {
        out.extend(unsafe { enumerate_interface_guid(source, &guid)? });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path.eq_ignore_ascii_case(&b.path));
    Ok(out)
}

pub fn enumerate_hci_radio_interfaces() -> Result<Vec<RadioInterface>, String> {
    let mut out = Vec::new();
    for interface in enumerate_radio_interfaces()? {
        let Ok(diagnostics) = diagnose_interface_path(&interface.path) else {
            continue;
        };
        if hci_transport_pipes_available(&diagnostics.classified) {
            out.push(interface);
        }
    }
    Ok(out)
}

pub fn diagnose_first_available() -> Result<Option<InterfaceDiagnostics>, String> {
    for interface in enumerate_hci_radio_interfaces()? {
        match diagnose_interface_path(&interface.path) {
            Ok(diagnostics) => return Ok(Some(diagnostics)),
            Err(e) => {
                eprintln!(
                    "[AokieRadio] Could not open candidate interface {}: {}",
                    interface.path, e
                );
            }
        }
    }
    Ok(None)
}

pub fn diagnose_interface_path(path: &str) -> Result<InterfaceDiagnostics, String> {
    unsafe {
        let device = DeviceHandle::open(path)?;
        let interface = WinUsbInterface::initialize(device.0)?;
        let mut diagnostics = query_interface(interface.0, path, 0, 0)?;

        if let Ok(associated) = WinUsbInterface::associated(interface.0, 0) {
            if let Ok(associated_diag) =
                query_interface_all_alternates(associated.0, path, 1, &SCO_ALT_SETTINGS_TO_PROBE)
            {
                diagnostics.pipes.extend(associated_diag.pipes);
                diagnostics.classified = classify_hci_pipes(&diagnostics.pipes);
            }
        }

        Ok(diagnostics)
    }
}

pub fn read_first_local_address() -> Result<Option<RadioAddress>, String> {
    for interface in enumerate_hci_radio_interfaces()? {
        match read_local_address(&interface.path) {
            Ok(local_address) => {
                return Ok(Some(RadioAddress {
                    device_path: interface.path,
                    local_address,
                }));
            }
            Err(e) => {
                eprintln!(
                    "[AokieRadio] Could not read local address from {}: {}",
                    interface.path, e
                );
            }
        }
    }
    Ok(None)
}

pub fn probe_first_controller() -> Result<Option<ControllerProbe>, String> {
    for interface in enumerate_hci_radio_interfaces()? {
        match probe_controller(&interface.path) {
            Ok(probe) => return Ok(Some(probe)),
            Err(e) => {
                eprintln!(
                    "[AokieRadio] Could not probe controller at {}: {}",
                    interface.path, e
                );
            }
        }
    }
    Ok(None)
}

pub fn probe_controller(path: &str) -> Result<ControllerProbe, String> {
    let transport = AokieHciTransport::open(path)?;
    transport.reset()?;
    transport.set_event_mask(0x3fffffff_ffffffff)?;
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

unsafe fn enumerate_interface_guid(
    source: InterfaceSource,
    guid: &GUID,
) -> Result<Vec<RadioInterface>, String> {
    let info_set = SetupDiGetClassDevsW(
        guid,
        null(),
        null_mut(),
        DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
    );
    if info_set == INVALID_HANDLE_VALUE as HDEVINFO {
        return Ok(Vec::new());
    }
    let info_set = DeviceInfoSet(info_set);
    let mut out = Vec::new();
    let mut index = 0;

    loop {
        let mut iface = zeroed::<SP_DEVICE_INTERFACE_DATA>();
        iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if SetupDiEnumDeviceInterfaces(info_set.0, null(), guid, index, &mut iface) == 0 {
            let err = GetLastError();
            if err == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(format!(
                "SetupDiEnumDeviceInterfaces({:?}, {}) failed: Win32 error {}",
                source, index, err
            ));
        }

        if let Some(path) = get_device_interface_path(info_set.0, &iface)? {
            out.push(RadioInterface { path, source });
        }
        index += 1;
    }

    Ok(out)
}

unsafe fn get_device_interface_path(
    info_set: HDEVINFO,
    iface: &SP_DEVICE_INTERFACE_DATA,
) -> Result<Option<String>, String> {
    let mut required = 0;
    let ok =
        SetupDiGetDeviceInterfaceDetailW(info_set, iface, null_mut(), 0, &mut required, null_mut());
    if ok == 0 && GetLastError() != ERROR_INSUFFICIENT_BUFFER {
        return Ok(None);
    }
    if required == 0 {
        return Ok(None);
    }

    let mut buffer = vec![0u8; required as usize];
    let detail = buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

    if SetupDiGetDeviceInterfaceDetailW(
        info_set,
        iface,
        detail,
        required,
        &mut required,
        null_mut(),
    ) == 0
    {
        return Err(format!(
            "SetupDiGetDeviceInterfaceDetailW failed: Win32 error {}",
            GetLastError()
        ));
    }

    let path_ptr = (*detail).DevicePath.as_ptr();
    let max_chars = (buffer.len().saturating_sub(size_of::<u32>())) / 2;
    let mut len = 0usize;
    while len < max_chars && *path_ptr.add(len) != 0 {
        len += 1;
    }
    Ok(Some(String::from_utf16_lossy(std::slice::from_raw_parts(
        path_ptr, len,
    ))))
}

unsafe fn query_interface(
    handle: WINUSB_INTERFACE_HANDLE,
    path: &str,
    interface_number: u8,
    alternate_setting: u8,
) -> Result<InterfaceDiagnostics, String> {
    let mut descriptor = zeroed::<USB_INTERFACE_DESCRIPTOR>();
    if WinUsb_QueryInterfaceSettings(handle, alternate_setting, &mut descriptor) == 0 {
        return Err(format!(
            "WinUsb_QueryInterfaceSettings failed: Win32 error {}",
            GetLastError()
        ));
    }

    let mut pipes = Vec::new();
    for index in 0..descriptor.bNumEndpoints {
        let mut pipe = zeroed::<WINUSB_PIPE_INFORMATION>();
        if WinUsb_QueryPipe(handle, alternate_setting, index, &mut pipe) == 0 {
            return Err(format!(
                "WinUsb_QueryPipe({}, {}) failed: Win32 error {}",
                alternate_setting,
                index,
                GetLastError()
            ));
        }
        pipes.push(pipe_info_from_winusb(pipe, alternate_setting));
    }

    let classified = classify_hci_pipes(&pipes);
    Ok(InterfaceDiagnostics {
        device_path: path.to_string(),
        interface_number,
        alternate_setting,
        pipes,
        classified,
    })
}

unsafe fn query_interface_all_alternates(
    handle: WINUSB_INTERFACE_HANDLE,
    path: &str,
    interface_number: u8,
    alternate_settings: &[u8],
) -> Result<InterfaceDiagnostics, String> {
    let mut diagnostics: Option<InterfaceDiagnostics> = None;
    for &alternate_setting in alternate_settings {
        match query_interface(handle, path, interface_number, alternate_setting) {
            Ok(mut queried) => {
                if let Some(existing) = diagnostics.as_mut() {
                    existing.pipes.append(&mut queried.pipes);
                    existing.classified = classify_hci_pipes(&existing.pipes);
                } else {
                    diagnostics = Some(queried);
                }
            }
            Err(_) if alternate_setting != 0 => {}
            Err(err) => return Err(err),
        }
    }

    diagnostics.ok_or_else(|| {
        format!(
            "WinUSB interface {} has no queryable alternate settings",
            interface_number
        )
    })
}

fn pipe_info_from_winusb(pipe: WINUSB_PIPE_INFORMATION, alternate_setting: u8) -> PipeInfo {
    PipeInfo {
        id: pipe.PipeId,
        kind: pipe_kind(pipe.PipeType),
        direction: if pipe.PipeId & 0x80 != 0 {
            PipeDirection::In
        } else {
            PipeDirection::Out
        },
        max_packet_size: pipe.MaximumPacketSize,
        interval: pipe.Interval,
        alternate_setting,
    }
}

unsafe fn set_pipe_timeout(
    handle: WINUSB_INTERFACE_HANDLE,
    pipe_id: u8,
    timeout_ms: u32,
) -> Result<(), String> {
    let timeout = timeout_ms;
    if WinUsb_SetPipePolicy(
        handle,
        pipe_id,
        PIPE_TRANSFER_TIMEOUT,
        size_of::<u32>() as u32,
        &timeout as *const u32 as *const _,
    ) == 0
    {
        return Err(format!(
            "WinUsb_SetPipePolicy(timeout) failed: Win32 error {}",
            GetLastError()
        ));
    }
    Ok(())
}

unsafe fn set_pipe_timeout_if_supported(
    handle: WINUSB_INTERFACE_HANDLE,
    pipe: PipeInfo,
    timeout_ms: u32,
) -> Result<(), String> {
    if pipe_timeout_supported(pipe.kind) {
        set_pipe_timeout(handle, pipe.id, timeout_ms)?;
    }
    Ok(())
}

fn pipe_timeout_supported(kind: PipeKind) -> bool {
    matches!(kind, PipeKind::Bulk | PipeKind::Interrupt)
}

fn hci_transport_pipes_available(pipes: &HciPipes) -> bool {
    pipes.event_in.is_some() && pipes.acl_in.is_some() && pipes.acl_out.is_some()
}

unsafe fn set_current_alternate_setting(
    handle: WINUSB_INTERFACE_HANDLE,
    alternate_setting: u8,
) -> Result<(), String> {
    if WinUsb_SetCurrentAlternateSetting(handle, alternate_setting) == 0 {
        return Err(format!(
            "WinUsb_SetCurrentAlternateSetting({}) failed: Win32 error {}",
            alternate_setting,
            GetLastError()
        ));
    }
    Ok(())
}

unsafe fn send_command(handle: WINUSB_INTERFACE_HANDLE, command: &[u8]) -> Result<(), String> {
    let setup = WINUSB_SETUP_PACKET {
        RequestType: 0x20,
        Request: 0,
        Value: 0,
        Index: 0,
        Length: command.len() as u16,
    };
    let mut transferred = 0;
    if WinUsb_ControlTransfer(
        handle,
        setup,
        command.as_ptr() as *mut u8,
        command.len() as u32,
        &mut transferred,
        null(),
    ) == 0
    {
        return Err(format!(
            "WinUsb_ControlTransfer(HCI command) failed: Win32 error {}",
            GetLastError()
        ));
    }
    if transferred != command.len() as u32 {
        return Err(format!(
            "WinUsb_ControlTransfer wrote {} of {} command bytes",
            transferred,
            command.len()
        ));
    }
    Ok(())
}

unsafe fn write_pipe(
    handle: WINUSB_INTERFACE_HANDLE,
    pipe_id: u8,
    label: &str,
    packet: &[u8],
) -> Result<(), String> {
    let mut transferred = 0;
    if WinUsb_WritePipe(
        handle,
        pipe_id,
        packet.as_ptr() as *mut u8,
        packet.len() as u32,
        &mut transferred,
        null(),
    ) == 0
    {
        return Err(format!(
            "WinUsb_WritePipe({}) failed: Win32 error {}",
            label,
            GetLastError()
        ));
    }
    if transferred != packet.len() as u32 {
        return Err(format!(
            "WinUsb_WritePipe({}) wrote {} of {} bytes",
            label,
            transferred,
            packet.len()
        ));
    }
    Ok(())
}

unsafe fn read_pipe(
    handle: WINUSB_INTERFACE_HANDLE,
    pipe_id: u8,
    label: &str,
    max_len: usize,
) -> Result<Vec<u8>, String> {
    let mut buffer = vec![0u8; max_len];
    let mut transferred = 0;
    if WinUsb_ReadPipe(
        handle,
        pipe_id,
        buffer.as_mut_ptr(),
        buffer.len() as u32,
        &mut transferred,
        null(),
    ) == 0
    {
        return Err(format!(
            "WinUsb_ReadPipe({}) failed: Win32 error {}",
            label,
            GetLastError()
        ));
    }
    buffer.truncate(transferred as usize);
    Ok(buffer)
}

unsafe fn register_isoch_buffer(
    handle: WINUSB_INTERFACE_HANDLE,
    pipe_id: u8,
    storage_len: usize,
) -> Result<WinUsbIsochBuffer, String> {
    if storage_len == 0 || storage_len > u32::MAX as usize {
        return Err(format!(
            "invalid isochronous buffer length: {}",
            storage_len
        ));
    }
    let mut storage = vec![0u8; storage_len];
    let mut buffer_handle = null_mut();
    if WinUsb_RegisterIsochBuffer(
        handle,
        pipe_id,
        storage.as_mut_ptr(),
        storage.len() as u32,
        &mut buffer_handle,
    ) == 0
    {
        return Err(format!(
            "WinUsb_RegisterIsochBuffer(0x{:02x}) failed: Win32 error {}",
            pipe_id,
            GetLastError()
        ));
    }
    let handle = NonNull::new(buffer_handle)
        .ok_or_else(|| "WinUsb_RegisterIsochBuffer returned a null handle".to_string())?;
    Ok(WinUsbIsochBuffer { handle, storage })
}

/// Cancel the in-flight overlapped I/O backed by `overlapped` and block
/// — BOUNDED — until cancellation provably completes (audit AK-01).
///
/// `device` is the REAL `CreateFileW` file handle: `CancelIoEx` is
/// documented against the file handle, and the opaque WinUSB interface
/// handle this function used to cast into it is NOT a kernel handle —
/// those cancels silently targeted nothing, so the following
/// `bWait=TRUE` drain could hang SCO teardown forever. `interface` is
/// the WinUSB interface handle, used only for
/// `WinUsb_GetOverlappedResult` (which deliberately takes the opaque
/// handle). Taking BOTH as distinct parameter types keeps the two from
/// being confused again at this seam.
///
/// Returns `true` when the OS-side pointer into `overlapped` is
/// PROVABLY retired — only then may the caller reuse the slot or free
/// its backing storage. Returns `false` when completion could not be
/// proven within the bound: the caller must treat the slot as poisoned
/// and LEAK its storage rather than reuse or free it (a kernel
/// completion write into freed memory corrupts the heap).
unsafe fn cancel_and_drain_overlapped(
    device: HANDLE,
    interface: WINUSB_INTERFACE_HANDLE,
    overlapped: &OVERLAPPED,
    operation: &str,
) -> bool {
    /// winerror.h ERROR_NOT_FOUND: no pending I/O matched — it already
    /// completed, which is fine (the drain below retires it instantly).
    const ERROR_NOT_FOUND_CANCEL: u32 = 1168;
    if CancelIoEx(device, overlapped) == 0 {
        let err = GetLastError();
        if err != ERROR_NOT_FOUND_CANCEL {
            eprintln!(
                "[AokieRadio] {} CancelIoEx failed: Win32 error {}",
                operation, err
            );
        }
    }
    // Bounded drain on the slot's completion event (every submit sets
    // overlapped.hEvent) instead of WinUsb_GetOverlappedResult(bWait=1),
    // which blocks indefinitely when a cancel is not honoured. 2s is far
    // beyond any real SCO completion latency.
    if !overlapped.hEvent.is_null() {
        let wait = WaitForSingleObject(overlapped.hEvent, 2_000);
        if wait != WAIT_OBJECT_0 {
            eprintln!(
                "[AokieRadio] {} cancellation did not complete within 2s (wait={}) — slot poisoned",
                operation, wait
            );
            return false;
        }
    }
    let mut transferred = 0;
    let ok = WinUsb_GetOverlappedResult(interface, overlapped, &mut transferred, 0);
    if ok == 0 {
        let err = GetLastError();
        // ERROR_OPERATION_ABORTED (995) is the expected outcome of a
        // cancel. ERROR_IO_INCOMPLETE (996) after a signalled event means
        // completion is NOT proven — poison the slot.
        if err == 996 {
            eprintln!(
                "[AokieRadio] {} drain: event signalled but I/O still incomplete — slot poisoned",
                operation
            );
            return false;
        }
        if err != 995 {
            eprintln!(
                "[AokieRadio] {} cancel/drain completed with Win32 error {}",
                operation, err
            );
        }
    }
    true
}

/// Prime the kernel's iso scheduling clock for `handle`. BTstack
/// (`hci_transport_h2_winusb.c:522,656,930,1428`) calls
/// `WinUsb_GetCurrentFrameNumber` immediately before every iso submit
/// AND inside every iso completion handler — *and discards the
/// returned values* in the ASAP path. Empirically, on a freshly
/// alt-set-2 SCO interface, `WinUsb_{Read,Write}IsochPipeAsap`
/// completes URBs with `transferred=0` and every microframe
/// descriptor at `len=0/status=0` until something forces the kernel
/// to register frame tracking on the device — and this call is what
/// does it. Pass the control (interface 0) handle, matching BTstack's
/// `usb_interface_0_handle` argument; the kernel just needs *some*
/// device-attached handle to start scheduling.
///
/// Failures are best-effort logged: this is a defensive prime, not a
/// fatal step. If the IOCTL fails the caller's iso submit will fail
/// anyway with a more specific error.
#[inline]
unsafe fn prime_iso_frame_clock(handle: WINUSB_INTERFACE_HANDLE) {
    let mut frame: u32 = 0;
    let mut timestamp: i64 = 0;
    let _ = WinUsb_GetCurrentFrameNumber(handle, &mut frame, &mut timestamp);
}

/// (Re-)submit a SCO IN ring slot. `continue_stream` follows
/// BTstack's policy: FALSE for every slot at startup, TRUE for every
/// re-submission after a slot completes. The caller owns that
/// distinction so this function doesn't have to know whether it's
/// being driven from the bootstrap fan-out or the drain loop.
///
/// `parent` is the control (interface 0) handle, used for the
/// `prime_iso_frame_clock` call that BTstack issues before every iso
/// read submit (`hci_transport_h2_winusb.c:522`).
fn submit_sco_in_slot(
    parent: WINUSB_INTERFACE_HANDLE,
    buffers: &mut ScoIsochBuffers,
    slot_idx: usize,
    continue_stream: bool,
) -> Result<(), String> {
    let slot_bytes = sco_in_slot_bytes(buffers.in_packet_size);
    let buffer_offset = (slot_idx * slot_bytes) as u32;
    let transfer_len = slot_bytes as u32;
    let continue_stream_bool: i32 = if continue_stream { 1 } else { 0 };

    {
        let slot = &mut buffers.in_slots[slot_idx];
        // Re-init OVERLAPPED in case this slot was previously used —
        // OS only writes Internal/InternalHigh during the I/O, but we
        // also need hEvent set and the rest cleared.
        *slot.overlapped = unsafe { zeroed::<OVERLAPPED>() };
        slot.overlapped.hEvent = slot.event.0;
        unsafe { ResetEvent(slot.event.0) };
        for descriptor in slot.descriptors.iter_mut() {
            descriptor.Offset = 0;
            descriptor.Length = 0;
            descriptor.Status = 0;
        }
    }

    let descriptors_ptr = buffers.in_slots[slot_idx].descriptors.as_mut_ptr();
    let overlapped_ptr = buffers.in_slots[slot_idx].overlapped.as_ref() as *const OVERLAPPED;
    unsafe { prime_iso_frame_clock(parent) };
    let submit_ok = unsafe {
        WinUsb_ReadIsochPipeAsap(
            buffers.in_buffer.handle.as_ptr(),
            buffer_offset,
            transfer_len,
            continue_stream_bool,
            SCO_IN_PACKETS_PER_SLOT as u32,
            descriptors_ptr,
            overlapped_ptr,
        )
    };
    if submit_ok == 0 {
        let err = unsafe { GetLastError() };
        if err != ERROR_IO_PENDING {
            return Err(format!(
                "WinUsb_ReadIsochPipeAsap(HCI SCO in slot {}) failed: Win32 error {}",
                slot_idx, err
            ));
        }
    }

    buffers.in_slots[slot_idx].in_use = true;
    buffers.in_pending += 1;
    Ok(())
}

/// Rate-limited log for per-slot SCO IN GetOverlappedResult failures.
/// We see these in steady state on Realtek dongles even when audio
/// data still arrives via the descriptors, so a per-event log would
/// drown the console at hundreds of lines per call.
fn log_sco_in_failure(slot_idx: usize, err: u32) {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
    static SUPPRESSED: AtomicU32 = AtomicU32::new(0);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_LOG_MS.load(Ordering::Relaxed);
    // One log line per 2 s, with a tally of how many we squelched
    // since the last line so the operator can still see if the
    // failure rate spikes.
    if now_ms.saturating_sub(last) >= 2_000 {
        let suppressed = SUPPRESSED.swap(0, Ordering::Relaxed);
        LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
        if suppressed > 0 {
            eprintln!(
                "[AokieRadio] SCO in slot {} GetOverlappedResult failed: Win32 error {} (+{} suppressed in last 2s)",
                slot_idx, err, suppressed
            );
        } else {
            eprintln!(
                "[AokieRadio] SCO in slot {} GetOverlappedResult failed: Win32 error {}",
                slot_idx, err
            );
        }
    } else {
        SUPPRESSED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Aggregated SCO OUT completion telemetry, emitted at most once every
/// 2 s. For iso WRITE the kernel reports `transferred` as the total
/// bytes accepted by the host controller across all packets. If our
/// HCI SCO packets are reaching the dongle, `transferred` matches
/// `packet.len()` (51 bytes for a 48-byte mSBC payload). If the
/// dongle is silently dropping our writes, `transferred` will read 0
/// or much less than expected.
fn log_sco_tx_drain_diag(transferred: u32, failed: bool) {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
    static AGG_COMPLETIONS: AtomicU32 = AtomicU32::new(0);
    static AGG_TOTAL_BYTES: AtomicU32 = AtomicU32::new(0);
    static AGG_FAILED: AtomicU32 = AtomicU32::new(0);
    static AGG_ZERO_BYTES: AtomicU32 = AtomicU32::new(0);

    AGG_COMPLETIONS.fetch_add(1, Ordering::Relaxed);
    AGG_TOTAL_BYTES.fetch_add(transferred, Ordering::Relaxed);
    if failed {
        AGG_FAILED.fetch_add(1, Ordering::Relaxed);
    }
    if transferred == 0 {
        AGG_ZERO_BYTES.fetch_add(1, Ordering::Relaxed);
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_LOG_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 2_000 {
        return;
    }
    LAST_LOG_MS.store(now_ms, Ordering::Relaxed);

    let cmp = AGG_COMPLETIONS.swap(0, Ordering::Relaxed);
    // Per Microsoft Learn ("Send USB Isochronous Transfers From a
    // WinUSB Desktop App"), `lpNumberOfBytesTransferred` is always 0
    // for isoch transfers — the docs say "the application should
    // assume that the bytes were transferred" if the Win32 result is
    // success. Logging "0 bytes total" was misleading us into thinking
    // every TX URB had failed when in fact the OS just doesn't expose
    // a per-URB byte count for iso writes. Drop those counters; only
    // GetOverlappedResult success/error and submit-side errors carry
    // signal here.
    let _ = AGG_TOTAL_BYTES.swap(0, Ordering::Relaxed);
    let _ = AGG_ZERO_BYTES.swap(0, Ordering::Relaxed);
    let failed = AGG_FAILED.swap(0, Ordering::Relaxed);
    eprintln!(
        "[AokieRadio] SCO TX completions (last 2s): {} URBs ({} GetOverlappedResult errors). NB: WinUSB iso writes always report bytes_transferred=0 — success/failure is per-URB only.",
        cmp, failed,
    );
}

/// One-line sample of an empty completion's descriptor states. Logged
/// at most once every 2 s — paired with the rate counter, this tells
/// us whether the controller is sending zero-length packets (Length=0
/// Status=0, normal "no audio") or USB-level errors (Status>>28 == 0xC).
fn log_sco_rx_empty_descriptors(
    slot_idx: usize,
    transferred: u32,
    descriptors: &[(u32, u32, u32)],
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_LOG_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 2_000 {
        return;
    }
    LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
    let mut parts = Vec::with_capacity(descriptors.len());
    for (i, (offset, len, status)) in descriptors.iter().enumerate() {
        parts.push(format!(
            "d{}={{offset={},len={},status=0x{:08x}}}",
            i, offset, len, status
        ));
    }
    eprintln!(
        "[AokieRadio] SCO RX empty completion sample: slot={} transferred={} {}",
        slot_idx,
        transferred,
        parts.join(" ")
    );
}

/// Aggregated SCO IN drain telemetry, emitted at most once every 2 s.
/// Counters are accumulated across `read_sco_isoch` calls so a quiet
/// SCO RX path produces a single periodic line instead of one per
/// loop iter (~hundreds/s). The breakdown distinguishes the silent
/// failure modes:
///   * `timeout`     — `WaitForSingleObject` on the head URB timed
///                     out (URB queued, kernel never signaled).
///   * `completed`   — URB event signaled at least once; subdivided
///                     into `with_data` vs `empty` so we can tell
///                     "controller delivered iso packets but they
///                     were zero-length / status!=0" from "URBs
///                     never completed at all".
///   * `ovr_fail`    — GetOverlappedResult returned non-INCOMPLETE
///                     error; descriptor scan still ran.
fn log_sco_rx_drain_diag(
    timeouts: u32,
    completions: u32,
    completion_with_data: u32,
    completion_empty: u32,
    failed_overlapped: u32,
    io_incomplete: u32,
    in_pending: usize,
    in_drain_idx: usize,
) {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
    static AGG_TIMEOUTS: AtomicU32 = AtomicU32::new(0);
    static AGG_COMPLETIONS: AtomicU32 = AtomicU32::new(0);
    static AGG_WITH_DATA: AtomicU32 = AtomicU32::new(0);
    static AGG_EMPTY: AtomicU32 = AtomicU32::new(0);
    static AGG_OVR_FAIL: AtomicU32 = AtomicU32::new(0);
    static AGG_IO_INCOMPLETE: AtomicU32 = AtomicU32::new(0);

    AGG_TIMEOUTS.fetch_add(timeouts, Ordering::Relaxed);
    AGG_COMPLETIONS.fetch_add(completions, Ordering::Relaxed);
    AGG_WITH_DATA.fetch_add(completion_with_data, Ordering::Relaxed);
    AGG_EMPTY.fetch_add(completion_empty, Ordering::Relaxed);
    AGG_OVR_FAIL.fetch_add(failed_overlapped, Ordering::Relaxed);
    AGG_IO_INCOMPLETE.fetch_add(io_incomplete, Ordering::Relaxed);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_LOG_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 2_000 {
        return;
    }
    LAST_LOG_MS.store(now_ms, Ordering::Relaxed);

    let to = AGG_TIMEOUTS.swap(0, Ordering::Relaxed);
    let cmp = AGG_COMPLETIONS.swap(0, Ordering::Relaxed);
    let wd = AGG_WITH_DATA.swap(0, Ordering::Relaxed);
    let em = AGG_EMPTY.swap(0, Ordering::Relaxed);
    let ovf = AGG_OVR_FAIL.swap(0, Ordering::Relaxed);
    let inc = AGG_IO_INCOMPLETE.swap(0, Ordering::Relaxed);
    eprintln!(
        "[AokieRadio] SCO RX drain (last 2s): timeouts={} completions={} (with_data={} empty={}) ovr_fail={} io_incomplete={} pending={} drain_idx={}",
        to, cmp, wd, em, ovf, inc, in_pending, in_drain_idx
    );
}

/// Drain finished slots out of the SCO out ring. With `blocking =
/// false` we only pop slots whose completion event is already
/// signaled. With `blocking = true` we wait up to 100 ms on the
/// oldest slot's event — used when the ring is full and we need to
/// free a slot before submitting a new write. On timeout we cancel
/// the slot's I/O and decrement `out_pending`; the next write
/// re-arms ContinueStream=FALSE automatically once the ring drains
/// to empty.
fn drain_sco_out_completions(
    buffers: &mut ScoIsochBuffers,
    device: HANDLE,
    interface: WINUSB_INTERFACE_HANDLE,
    blocking: bool,
) -> Result<(), String> {
    let ring_len = buffers.out_slots.len();
    while buffers.out_pending > 0 {
        let drain_idx = buffers.out_drain_idx;
        let slot_in_use = buffers.out_slots[drain_idx].in_use;
        if !slot_in_use {
            // Should never happen if pending count is correct, but
            // skip past stale slots defensively.
            buffers.out_drain_idx = (drain_idx + 1) % ring_len;
            continue;
        }

        let timeout = if blocking { 100 } else { 0 };
        let event = buffers.out_slots[drain_idx].event.0;
        let wait_result = unsafe { WaitForSingleObject(event, timeout) };
        match wait_result {
            WAIT_OBJECT_0 => {
                let mut transferred = 0;
                let ok = unsafe {
                    WinUsb_GetOverlappedResult(
                        interface,
                        buffers.out_slots[drain_idx].overlapped.as_ref(),
                        &mut transferred,
                        0,
                    )
                };
                if ok == 0 {
                    let err = unsafe { GetLastError() };
                    eprintln!(
                        "[AokieRadio] SCO out slot {} GetOverlappedResult failed: Win32 error {}",
                        drain_idx, err
                    );
                    log_sco_tx_drain_diag(0, true);
                } else {
                    log_sco_tx_drain_diag(transferred, false);
                }
                buffers.out_slots[drain_idx].in_use = false;
                buffers.out_pending -= 1;
                buffers.out_drain_idx = (drain_idx + 1) % ring_len;
            }
            WAIT_TIMEOUT if !blocking => {
                // Oldest slot still in flight and we're not willing
                // to wait. Stop draining; subsequent slots can't be
                // older.
                break;
            }
            WAIT_TIMEOUT => {
                // Blocking wait timed out. Cancel the I/O and drain the
                // cancellation (bounded) so the OS-side pointer to our
                // boxed OVERLAPPED is retired before we re-use the slot.
                let retired = unsafe {
                    cancel_and_drain_overlapped(
                        device,
                        interface,
                        buffers.out_slots[drain_idx].overlapped.as_ref(),
                        "HCI SCO isoch write (slot)",
                    )
                };
                if !retired {
                    // Completion unproven (audit AK-01): the slot may still
                    // be written by the kernel — NEVER mark it free. Leave
                    // it in_use so teardown re-attempts (and leaks the ring
                    // if still unproven) instead of reusing the storage.
                    return Err(format!(
                        "SCO out slot {} could not be cancelled — stream halted pending teardown",
                        drain_idx
                    ));
                }
                buffers.out_slots[drain_idx].in_use = false;
                buffers.out_pending -= 1;
                buffers.out_drain_idx = (drain_idx + 1) % ring_len;
            }
            _ => {
                let err = unsafe { GetLastError() };
                return Err(format!(
                    "WaitForSingleObject(SCO out slot {}) failed: Win32 error {}",
                    drain_idx, err
                ));
            }
        }
    }
    Ok(())
}

pub struct AokieHciTransport {
    _device: DeviceHandle,
    control: WinUsbInterface,
    associated: Option<WinUsbInterface>,
    pipes: TransportPipeSet,
    sco_isoch_buffers: Option<ScoIsochBuffers>,
    sco_transport_config: Option<ScoTransportConfig>,
    /// SCO alt settings actually exposed by interface 1 on this dongle.
    /// Populated once during `open()`. We use this to fall back from the
    /// spec-correct alt (e.g. alt 6 for mSBC, MPS=63) to whatever the
    /// device exposes when it doesn't implement the canonical mapping.
    /// Cheap dongles routinely ship without alt 6 even though they
    /// advertise mSBC over HFP.
    available_sco_alts: Vec<u8>,
    dump: PacketDump,
    /// Serializes `command_complete` so two callers from different
    /// threads cannot interleave their command-and-response pairs and
    /// steal each other's HCI Command Complete events. The methods are
    /// `&self` so the compiler won't catch the race for us.
    ///
    /// Note: this only protects `command_complete`. Direct `read_event`
    /// calls (used by the manager's event loop) remain unsynchronized,
    /// which is fine as long as the event loop and `command_complete`
    /// don't run on the same transport concurrently — currently they
    /// don't, but the latch makes the API safe to share before that
    /// invariant is enforced at a higher level.
    command_lock: std::sync::Mutex<()>,
    /// Async events that arrived on the HCI event endpoint while we
    /// were waiting for a Command Complete response. We push them here
    /// instead of dropping them on the floor so the main runtime loop
    /// (`read_event`) sees them on its next tick. Without this buffer,
    /// any ConnectionComplete / IO_Capability_Request /
    /// User_Confirmation_Request that races a pairing reply round-trip
    /// gets lost — which manifests as "remote BD_ADDR is all zeros"
    /// and missed call-setup transitions.
    deferred_events: std::sync::Mutex<VecDeque<Vec<u8>>>,
}

impl AokieHciTransport {
    pub fn open_first() -> Result<Option<Self>, String> {
        for interface in enumerate_hci_radio_interfaces()? {
            match Self::open(&interface.path) {
                Ok(transport) => return Ok(Some(transport)),
                Err(e) => {
                    eprintln!(
                        "[AokieRadio] Could not open HCI transport at {}: {}",
                        interface.path, e
                    );
                }
            }
        }
        Ok(None)
    }

    pub fn open(path: &str) -> Result<Self, String> {
        unsafe { Self::open_inner(path) }
    }

    unsafe fn open_inner(path: &str) -> Result<Self, String> {
        let device = DeviceHandle::open(path)?;
        let control = WinUsbInterface::initialize(device.0)?;
        let control_diag = query_interface(control.0, path, 0, 0)?;
        let associated = WinUsbInterface::associated(control.0, 0).ok();
        let associated_diag = associated
            .as_ref()
            .and_then(|associated| query_interface(associated.0, path, 1, 0).ok());

        // Ground-truth diagnostic: walk every SCO alt setting (0-6) the
        // device exposes on interface 1 (the audio interface), and log
        // each one's endpoints with their MPS and interval. The
        // Bluetooth Core spec USB Transport Layer table reserves alt 6
        // (MPS=63) for one mSBC voice channel — devices that don't
        // expose alt 6 will silently fall back to alt 2 (MPS=17, sized
        // for CVSD-16-bit), which routes mSBC frames onto a
        // CVSD-shaped pipe. Logging this once at startup tells us
        // whether each dongle actually has the mSBC alt.
        let mut available_sco_alts: Vec<u8> = Vec::new();
        if let Some(associated) = associated.as_ref() {
            for alt in SCO_ALT_SETTINGS_TO_PROBE {
                match query_interface(associated.0, path, 1, alt) {
                    Ok(diag) => {
                        available_sco_alts.push(alt);
                        if diag.pipes.is_empty() {
                            eprintln!(
                                "[AokieRadio] SCO alt-setting probe: alt {} present, no isoch endpoints",
                                alt,
                            );
                        } else {
                            for pipe in &diag.pipes {
                                eprintln!(
                                    "[AokieRadio] SCO alt-setting probe: alt {} ep=0x{:02x} {:?} {:?} mps={} interval={}",
                                    alt,
                                    pipe.id,
                                    pipe.kind,
                                    pipe.direction,
                                    pipe.max_packet_size,
                                    pipe.interval,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[AokieRadio] SCO alt-setting probe: alt {} unavailable ({})",
                            alt, e,
                        );
                    }
                }
            }
        }

        let pipes = classify_transport_pipes(&control_diag, associated_diag.as_ref());
        let event_in = pipes
            .event_in
            .ok_or_else(|| "WinUSB interface has no HCI event endpoint".to_string())?;

        set_pipe_timeout(
            interface_handle(&control, associated.as_ref(), event_in.slot)?,
            event_in.info.id,
            5000,
        )?;
        if let Some(acl_in) = pipes.acl_in {
            set_pipe_timeout(
                interface_handle(&control, associated.as_ref(), acl_in.slot)?,
                acl_in.info.id,
                5000,
            )?;
        }

        Ok(Self {
            _device: device,
            control,
            associated,
            pipes,
            sco_isoch_buffers: None,
            sco_transport_config: None,
            available_sco_alts,
            dump: PacketDump::from_env(),
            command_lock: std::sync::Mutex::new(()),
            deferred_events: std::sync::Mutex::new(VecDeque::new()),
        })
    }

    /// Drop any URBs the kernel has buffered or in flight on the HCI
    /// command/event and ACL pipes from a previous session. Without
    /// this, an HCI ACL packet that was queued by the controller
    /// while a prior runtime owned the WinUSB handle (or that landed
    /// after our previous `WinUsb_Free`) is the first thing the next
    /// read returns — our parser sees garbage prefix bytes ahead of
    /// the next real L2CAP signaling frame. Concretely we've seen
    /// this strand a fresh-pair SDP `ConnectionRequest` behind 2
    /// leftover bytes (`88 e0 ...`) on the first ACL read after
    /// pairing, which breaks SLC by 5s timeout. Power-cycling the
    /// dongle masks this; calling `flush_in_pipes()` after `open()`
    /// makes app restart behave like a power-cycle.
    ///
    /// We deliberately skip the SCO IN pipe even though it's also an
    /// IN endpoint. SCO is isochronous and the runtime arms it
    /// fresh per-call via `SetCurrentAlternateSetting` +
    /// `RegisterIsochBuffer` (BTstack `usb_sco_start` parity), so
    /// there's never a stale-URB hazard between sessions. Calling
    /// `WinUsb_AbortPipe` / `WinUsb_ResetPipe` on the iso pipe
    /// before it has a registered iso buffer leaves WinUSB in a
    /// state where the per-call iso bring-up silently never
    /// completes — the SCO link request is accepted but the
    /// `Synchronous_Connection_Complete` event never delivers
    /// audio. (`WinUsb_FlushPipe` also returns `Win32 error 87 =
    /// ERROR_INVALID_PARAMETER` on iso pipes — it's only defined
    /// for bulk.)
    ///
    /// Sequence per pipe: AbortPipe (cancel pending transfers) →
    /// ResetPipe (clear data toggle / any USB stall) → FlushPipe
    /// (drop kernel-buffered IN data). Errors are non-fatal and
    /// logged — the most common failure is "no transfers in
    /// flight," which is fine.
    pub fn flush_in_pipes(&self) -> Result<(), String> {
        let pipes: [(&str, Option<PipeEndpoint>); 2] = [
            ("HCI event in", self.pipes.event_in),
            ("HCI ACL in", self.pipes.acl_in),
        ];
        for (label, endpoint) in pipes {
            let Some(endpoint) = endpoint else { continue };
            let handle = self.pipe_handle(endpoint.slot)?;
            unsafe {
                if WinUsb_AbortPipe(handle, endpoint.info.id) == 0 {
                    eprintln!(
                        "[AokieRadio] {} flush: AbortPipe failed: Win32 error {}",
                        label,
                        GetLastError()
                    );
                }
                if WinUsb_ResetPipe(handle, endpoint.info.id) == 0 {
                    eprintln!(
                        "[AokieRadio] {} flush: ResetPipe failed: Win32 error {}",
                        label,
                        GetLastError()
                    );
                }
                if WinUsb_FlushPipe(handle, endpoint.info.id) == 0 {
                    eprintln!(
                        "[AokieRadio] {} flush: FlushPipe failed: Win32 error {}",
                        label,
                        GetLastError()
                    );
                }
            }
        }
        Ok(())
    }

    /// Drop kernel-buffered IN data on the ACL pipe only. Used after
    /// a mid-session ACL DisconnectionComplete: the controller can
    /// have a couple of bytes of L2CAP signalling for the dead handle
    /// still queued in the bulk-IN ring, and those prepend to the
    /// first read of the next ACL session — exactly the `88 e0 0c 20
    /// ...` 2-byte misalignment the existing `flush_in_pipes()`
    /// already cures at `open()` time.
    ///
    /// Differences from `flush_in_pipes()`:
    ///   * ACL pipe only — leaves the event pipe alone so a
    ///     fast-reconnecting peer's Connection Request can't be
    ///     dropped under us.
    ///   * `WinUsb_FlushPipe` only (no AbortPipe / ResetPipe). The
    ///     latter two clear the data-toggle bit, which mid-session
    ///     desyncs us from the device's toggle state and stalls the
    ///     next bulk transfer. FlushPipe just drops the kernel ring
    ///     contents without touching transfer state.
    pub fn flush_acl_in_pipe(&self) -> Result<(), String> {
        let Some(endpoint) = self.pipes.acl_in else {
            return Ok(());
        };
        let handle = self.pipe_handle(endpoint.slot)?;
        unsafe {
            if WinUsb_FlushPipe(handle, endpoint.info.id) == 0 {
                eprintln!(
                    "[AokieRadio] HCI ACL in post-disconnect flush: FlushPipe failed: Win32 error {}",
                    GetLastError()
                );
            }
        }
        Ok(())
    }

    pub fn command_complete(&self, command: &[u8], opcode: u16) -> Result<Vec<u8>, String> {
        // Hold the latch across the whole write+read pair. Releasing it
        // between the two would let a second caller's command sneak in
        // and have its Command Complete consumed by our reader. The
        // lock itself never blocks long — Command Complete responses
        // arrive in a few ms — so the contention cost is negligible.
        let _guard = self
            .command_lock
            .lock()
            .map_err(|e| format!("HCI command lock poisoned: {}", e))?;
        self.write_command(command)?;
        self.read_command_complete(opcode)
    }

    pub fn command_return_params(&self, command: &[u8], opcode: u16) -> Result<Vec<u8>, String> {
        let event = self.command_complete(command, opcode)?;
        Ok(crate::aokie_radio::hci::parse_command_complete(&event, opcode)?.to_vec())
    }

    pub fn reset(&self) -> Result<(), String> {
        let params = self.command_return_params(
            &crate::aokie_radio::hci::reset_command(),
            crate::aokie_radio::hci::OPCODE_RESET,
        )?;
        crate::aokie_radio::hci::expect_status_ok(&params, "HCI Reset")
    }

    pub fn set_event_mask(&self, mask: u64) -> Result<(), String> {
        let command = crate::aokie_radio::hci::set_event_mask_command(mask);
        let params =
            self.command_return_params(&command, crate::aokie_radio::hci::OPCODE_SET_EVENT_MASK)?;
        crate::aokie_radio::hci::expect_status_ok(&params, "Set Event Mask")
    }

    pub fn read_local_version(&self) -> Result<crate::aokie_radio::hci::LocalVersion, String> {
        let params = self.command_return_params(
            &crate::aokie_radio::hci::read_local_version_information_command(),
            crate::aokie_radio::hci::OPCODE_READ_LOCAL_VERSION_INFORMATION,
        )?;
        crate::aokie_radio::hci::parse_local_version_return(&params)
    }

    pub fn read_local_supported_features(&self) -> Result<[u8; 8], String> {
        let params = self.command_return_params(
            &crate::aokie_radio::hci::read_local_supported_features_command(),
            crate::aokie_radio::hci::OPCODE_READ_LOCAL_SUPPORTED_FEATURES,
        )?;
        crate::aokie_radio::hci::parse_local_supported_features_return(&params)
    }

    pub fn read_buffer_size(&self) -> Result<crate::aokie_radio::hci::BufferSize, String> {
        let params = self.command_return_params(
            &crate::aokie_radio::hci::read_buffer_size_command(),
            crate::aokie_radio::hci::OPCODE_READ_BUFFER_SIZE,
        )?;
        crate::aokie_radio::hci::parse_buffer_size_return(&params)
    }

    pub fn read_bd_addr(&self) -> Result<String, String> {
        let params = self.command_return_params(
            &crate::aokie_radio::hci::read_bd_addr_command(),
            crate::aokie_radio::hci::OPCODE_READ_BD_ADDR,
        )?;
        crate::aokie_radio::hci::parse_read_bd_addr_return(&params)
    }

    pub fn write_command(&self, command: &[u8]) -> Result<(), String> {
        self.dump.log("cmd >", command);
        unsafe { send_command(self.control.0, command) }
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
        let event_in = self
            .pipes
            .event_in
            .ok_or_else(|| "WinUSB interface has no HCI event endpoint".to_string())?;
        let packet = unsafe {
            read_pipe(
                self.pipe_handle(event_in.slot)?,
                event_in.info.id,
                "HCI event",
                260,
            )?
        };
        self.dump.log("evt <", &packet);
        Ok(packet)
    }

    pub fn set_read_timeouts(
        &self,
        event_timeout_ms: u32,
        acl_timeout_ms: Option<u32>,
        sco_timeout_ms: Option<u32>,
    ) -> Result<(), String> {
        if let Some(event_in) = self.pipes.event_in {
            unsafe {
                set_pipe_timeout_if_supported(
                    self.pipe_handle(event_in.slot)?,
                    event_in.info,
                    event_timeout_ms,
                )?
            };
        }
        if let (Some(acl_in), Some(timeout_ms)) = (self.pipes.acl_in, acl_timeout_ms) {
            unsafe {
                set_pipe_timeout_if_supported(
                    self.pipe_handle(acl_in.slot)?,
                    acl_in.info,
                    timeout_ms,
                )?
            };
        }
        if let (Some(sco_in), Some(timeout_ms)) = (self.pipes.sco_in, sco_timeout_ms) {
            unsafe {
                set_pipe_timeout_if_supported(
                    self.pipe_handle(sco_in.slot)?,
                    sco_in.info,
                    timeout_ms,
                )?
            };
        }
        Ok(())
    }

    pub fn configure_sco_alt_setting(
        &mut self,
        voice_setting: u16,
        connection_count: usize,
    ) -> Result<(), String> {
        let Some(requested_alt) = sco_alt_setting_for_voice(voice_setting, connection_count) else {
            return Ok(());
        };
        // Probe-aware fallback: the spec-correct alt for mSBC is 6
        // (MPS=63), but cheap dongles routinely advertise mSBC over HFP
        // without exposing alt 6 on the USB descriptor. When the
        // requested alt isn't in the probe set, fall back to the first
        // 16-bit CVSD alt (alt 2 / MPS=17) so SetCurrentAlternateSetting
        // doesn't fail with Win32 1168 and crash the runtime. Without
        // this fallback the runtime restart-loops on every dongle that
        // omits alt 6 — and most do.
        let requested_alt = if !self.available_sco_alts.is_empty()
            && !self.available_sco_alts.contains(&requested_alt)
        {
            let fallback = SCO_ALT_SETTINGS_16_BIT
                .iter()
                .copied()
                .find(|alt| self.available_sco_alts.contains(alt));
            match fallback {
                Some(alt) => {
                    eprintln!(
                        "[AokieRadio] SCO alt {} not exposed by dongle (available={:?}); falling back to alt {}",
                        requested_alt, self.available_sco_alts, alt,
                    );
                    alt
                }
                None => {
                    eprintln!(
                        "[AokieRadio] SCO alt {} not exposed and no 16-bit fallback available (probe={:?}); skipping alt-config",
                        requested_alt, self.available_sco_alts,
                    );
                    return Ok(());
                }
            }
        } else {
            requested_alt
        };

        // If a previous SCO config is still up (no DisconnectionComplete
        // arrived between calls, or this is a re-configuration), tear it
        // down so the OVERLAPPED boxes WinUSB still holds pointers to
        // are cancelled and drained before we drop them. Otherwise the
        // OS can write completion data to freed heap memory.
        if self.sco_isoch_buffers.is_some() {
            self.disable_sco_alt_setting()?;
        }

        let Some(associated) = self.associated.as_ref() else {
            return Ok(());
        };

        unsafe { set_current_alternate_setting(associated.0, requested_alt)? };
        self.pipes.sco_in = None;
        self.pipes.sco_out = None;

        let diagnostics = unsafe { query_interface(associated.0, "", 1, requested_alt)? };
        add_transport_pipes(
            &mut self.pipes,
            InterfaceSlot::Associated,
            &diagnostics.pipes,
        );

        if let Some(sco_in) = self.pipes.sco_in {
            unsafe {
                set_pipe_timeout_if_supported(self.pipe_handle(sco_in.slot)?, sco_in.info, 1000)?
            };
        }
        self.sco_isoch_buffers = self.register_sco_isoch_buffers()?;
        // Bring-up prime: BTstack's `usb_sco_start` calls
        // `BTstack_WinUsb_GetCurrentFrameNumber` immediately after
        // `WinUsb_SetCurrentAlternateSetting` + buffer registration
        // (`hci_transport_h2_winusb.c:930`) so the kernel starts
        // tracking iso frames against the device before the first
        // `WriteIsochPipeAsap` / `ReadIsochPipeAsap` lands. Without
        // this prime, freshly-set-up iso pipes complete every URB
        // with `transferred=0` and every microframe descriptor at
        // `len=0/status=0` — exactly the dead-on-the-wire fingerprint
        // we hit on both Realtek and Broadcom dongles.
        unsafe { prime_iso_frame_clock(self.control.0) };
        self.sco_transport_config = Some(sco_transport_config(
            requested_alt,
            self.pipes.sco_in,
            self.pipes.sco_out,
            self.sco_isoch_buffers.is_some(),
        ));
        Ok(())
    }

    pub fn disable_sco_alt_setting(&mut self) -> Result<(), String> {
        let Some(associated) = self.associated.as_ref() else {
            return Ok(());
        };
        // Cancel every pending SCO transfer before tearing down the
        // rings — the OS still holds pointers into our boxed
        // OVERLAPPED structs, and dropping them while I/O is in
        // flight would corrupt heap memory the next time the OS
        // wrote completion data into them.
        let mut all_retired = true;
        if let Some(buffers) = self.sco_isoch_buffers.as_mut() {
            let device = self._device.0;
            let interface = associated.0;
            for slot in &mut buffers.out_slots {
                if slot.in_use {
                    let retired = unsafe {
                        cancel_and_drain_overlapped(
                            device,
                            interface,
                            slot.overlapped.as_ref(),
                            "HCI SCO isoch write (teardown)",
                        )
                    };
                    all_retired &= retired;
                    slot.in_use = false;
                }
            }
            buffers.out_pending = 0;

            for slot in &mut buffers.in_slots {
                if slot.in_use {
                    let retired = unsafe {
                        cancel_and_drain_overlapped(
                            device,
                            interface,
                            slot.overlapped.as_ref(),
                            "HCI SCO isoch read (teardown)",
                        )
                    };
                    all_retired &= retired;
                    slot.in_use = false;
                }
            }
            buffers.in_pending = 0;
        }
        if all_retired {
            self.sco_isoch_buffers = None;
        } else if let Some(buffers) = self.sco_isoch_buffers.take() {
            // Completion unproven for at least one slot (audit AK-01): the
            // kernel may still write into this ring's OVERLAPPED/storage.
            // Deliberately LEAK the whole ring (a few KiB, once, on an
            // already-pathological teardown) rather than free memory the
            // OS can still touch — that is the use-after-free path.
            eprintln!(
                "[AokieRadio] SCO teardown: cancellation unproven — leaking the isoch ring instead of freeing it"
            );
            std::mem::forget(buffers);
        }
        unsafe { set_current_alternate_setting(associated.0, 0)? };
        self.pipes.sco_in = None;
        self.pipes.sco_out = None;
        self.sco_transport_config = None;
        Ok(())
    }

    pub fn sco_transport_config(&self) -> Option<ScoTransportConfig> {
        self.sco_transport_config
    }

    /// Whether the dongle exposes a usable mSBC SCO alt-setting under
    /// the BTstack-parity wire shape we adopted in `bf95183`: voice
    /// setting 0x0043 (transparent + 8-bit input) + alt 1 (MPS=9) for
    /// one connection. The 24-byte HCI SCO USB-transport payload
    /// override (`hci.c:4877-4885`) lets mSBC ride alt 1 — a 60-byte
    /// frame splits across 2.5 HCI packets and the H2 sync triplet
    /// drives reassembly. Pre-bf95183 we required alt 6 (MPS=63),
    /// which Broadcom 21ec doesn't expose; that gate denied mSBC
    /// even on dongles where the BTstack-parity path works fine.
    pub fn supports_msbc_alt_setting(&self) -> bool {
        SCO_ALT_SETTINGS_8_BIT
            .iter()
            .any(|alt| self.available_sco_alts.contains(alt))
    }

    fn register_sco_isoch_buffers(&self) -> Result<Option<ScoIsochBuffers>, String> {
        let Some(associated) = self.associated.as_ref() else {
            return Ok(None);
        };
        let Some(sco_in) = self.pipes.sco_in else {
            return Ok(None);
        };
        let Some(sco_out) = self.pipes.sco_out else {
            return Ok(None);
        };

        let in_buffer_len = sco_isoch_buffer_len(sco_in.info.max_packet_size);
        if in_buffer_len == 0 {
            return Ok(None);
        }
        let out_packet_size = sco_out.info.max_packet_size;
        let out_slot_stride = align_up(SCO_OUT_MAX_HCI_PACKET_BYTES, out_packet_size as usize);
        let out_buffer_len = SCO_OUT_RING_SLOTS * out_slot_stride;
        eprintln!(
            "[AokieRadio] SCO OUT iso buffer: out_mps={} slot_stride={} ring_slots={} buffer_len={}",
            out_packet_size, out_slot_stride, SCO_OUT_RING_SLOTS, out_buffer_len,
        );

        let in_buffer =
            unsafe { register_isoch_buffer(associated.0, sco_in.info.id, in_buffer_len)? };
        let out_buffer =
            unsafe { register_isoch_buffer(associated.0, sco_out.info.id, out_buffer_len)? };

        let mut out_slots = Vec::with_capacity(SCO_OUT_RING_SLOTS);
        for _ in 0..SCO_OUT_RING_SLOTS {
            out_slots.push(ScoOutSlot {
                overlapped: Box::new(unsafe { zeroed::<OVERLAPPED>() }),
                event: unsafe { EventHandle::manual_reset()? },
                in_use: false,
            });
        }

        let mut in_slots = Vec::with_capacity(SCO_IN_RING_SLOTS);
        for _ in 0..SCO_IN_RING_SLOTS {
            let descriptors = (0..SCO_IN_PACKETS_PER_SLOT)
                .map(|_| USBD_ISO_PACKET_DESCRIPTOR {
                    Offset: 0,
                    Length: 0,
                    Status: 0,
                })
                .collect::<Vec<_>>()
                .into_boxed_slice();
            in_slots.push(ScoInSlot {
                overlapped: Box::new(unsafe { zeroed::<OVERLAPPED>() }),
                event: unsafe { EventHandle::manual_reset()? },
                descriptors,
                in_use: false,
            });
        }

        Ok(Some(ScoIsochBuffers {
            in_buffer,
            out_buffer,
            in_slots,
            in_drain_idx: 0,
            in_pending: 0,
            in_packet_size: sco_in.info.max_packet_size,
            out_slots,
            out_write_idx: 0,
            out_drain_idx: 0,
            out_pending: 0,
            out_packet_size,
            out_slot_stride,
        }))
    }

    pub fn read_command_complete(&self, opcode: u16) -> Result<Vec<u8>, String> {
        // Read directly from the USB pipe (NOT via `read_event`) so we
        // never accidentally consume an event we previously deferred —
        // a deferred event by definition is not the command_complete
        // we're looking for, otherwise we'd have returned it already.
        //
        // The cap on `ignored_events` is purely a safety net so a stuck
        // controller (or a parse-error path that keeps retriggering) can
        // never spin this loop forever. 4096 is far above anything we'd
        // ever see in practice — a healthy init sends a handful of
        // commands and a controller hosting a couple of inquiries
        // produces at most a dozen events between any two of ours. The
        // pre-fix value of 64 was small enough that an event flood
        // during pairing (User Confirmation Request bursts on shared
        // dongles) would surface as a spurious "command failed" error
        // instead of completing.
        const MAX_IGNORED_EVENTS: usize = 4096;
        let mut ignored_events = 0;
        loop {
            let event_in = self
                .pipes
                .event_in
                .ok_or_else(|| "WinUSB interface has no HCI event endpoint".to_string())?;
            let event = unsafe {
                read_pipe(
                    self.pipe_handle(event_in.slot)?,
                    event_in.info.id,
                    "HCI event",
                    260,
                )?
            };
            self.dump.log("evt <", &event);
            match crate::aokie_radio::hci::parse_command_complete(&event, opcode) {
                Ok(_) => return Ok(event),
                Err(_) if ignored_events < MAX_IGNORED_EVENTS => {
                    // Stash the event so the main loop can dispatch it
                    // on its next read_event() tick. Otherwise async
                    // events that race a command_complete (Connection
                    // Complete, IO Capability Request, etc.) are lost.
                    if let Ok(mut queue) = self.deferred_events.lock() {
                        queue.push_back(event);
                    }
                    ignored_events += 1;
                }
                Err(e) => {
                    return Err(format!(
                        "{} (after deferring {} events while waiting for opcode 0x{:04x})",
                        e, ignored_events, opcode
                    ))
                }
            }
        }
    }

    pub fn write_acl(&self, packet: &[u8]) -> Result<(), String> {
        let endpoint = self
            .pipes
            .acl_out
            .ok_or_else(|| "WinUSB interface has no HCI ACL out endpoint".to_string())?;
        self.dump.log("acl >", packet);
        unsafe {
            write_pipe(
                self.pipe_handle(endpoint.slot)?,
                endpoint.info.id,
                "HCI ACL out",
                packet,
            )
        }
    }

    pub fn read_acl(&self, max_len: usize) -> Result<Vec<u8>, String> {
        let endpoint = self
            .pipes
            .acl_in
            .ok_or_else(|| "WinUSB interface has no HCI ACL in endpoint".to_string())?;
        let packet = unsafe {
            read_pipe(
                self.pipe_handle(endpoint.slot)?,
                endpoint.info.id,
                "HCI ACL in",
                max_len,
            )?
        };
        self.dump.log("acl <", &packet);
        Ok(packet)
    }

    pub fn write_sco(&mut self, packet: &[u8]) -> Result<(), String> {
        let endpoint = self
            .pipes
            .sco_out
            .ok_or_else(|| "WinUSB interface has no HCI SCO out endpoint".to_string())?;
        self.dump.log("sco >", packet);

        if endpoint.info.kind == PipeKind::Isochronous {
            return self.write_sco_isoch(packet);
        }

        unsafe {
            write_pipe(
                self.pipe_handle(endpoint.slot)?,
                endpoint.info.id,
                "HCI SCO out",
                packet,
            )
        }
    }

    pub fn read_sco(&mut self, max_len: usize) -> Result<Vec<u8>, String> {
        let endpoint = self
            .pipes
            .sco_in
            .ok_or_else(|| "WinUSB interface has no HCI SCO in endpoint".to_string())?;
        if endpoint.info.kind == PipeKind::Isochronous {
            let packet = self.read_sco_isoch(max_len, endpoint.info.max_packet_size)?;
            self.dump.log("sco <", &packet);
            return Ok(packet);
        }

        let packet = unsafe {
            read_pipe(
                self.pipe_handle(endpoint.slot)?,
                endpoint.info.id,
                "HCI SCO in",
                max_len,
            )?
        };
        self.dump.log("sco <", &packet);
        Ok(packet)
    }

    fn read_sco_isoch(
        &mut self,
        _max_len: usize,
        _max_packet_size: u16,
    ) -> Result<Vec<u8>, String> {
        let interface = self.pipe_handle(InterfaceSlot::Associated)?;
        // Control handle for the iso-frame-clock priming that
        // `submit_sco_in_slot` issues before each `ReadIsochPipeAsap`.
        // BTstack passes its `usb_interface_0_handle` here; we mirror
        // that with the control slot.
        let parent = self.pipe_handle(InterfaceSlot::Control)?;
        let Some(buffers) = self.sco_isoch_buffers.as_mut() else {
            return Err("HCI SCO isochronous buffers are not registered".to_string());
        };

        // Bootstrap the ring on first read (or re-bootstrap after a
        // teardown that left it empty).
        //
        // BTstack reference: `usb_submit_sco_in_transfer_asap(i, 0)`
        // (`hci_transport_h2_winusb.c:940`) — every bootstrap slot uses
        // `ContinueStream = FALSE`. The earlier hypothesis here ("FALSE
        // in a tight loop cancels each predecessor URB") was wrong;
        // BTstack does exactly that and successfully drives iso both
        // ways on dongles where our previous TRUE-chained bootstrap
        // produced zero-byte URBs (Broadcom 21ec, 2026-04-29 A/B test
        // against the C bridge in worktree `aokie-05e3d51`). Mirror it.
        if buffers.in_pending == 0 {
            buffers.in_drain_idx = 0;
            let total_slots = buffers.in_slots.len();
            let mut last_err: Option<String> = None;
            let mut failed = 0usize;
            let mut submitted = 0usize;
            for slot_idx in 0..total_slots {
                if let Err(e) = submit_sco_in_slot(parent, buffers, slot_idx, false) {
                    eprintln!(
                        "[AokieRadio] SCO in slot {} bootstrap submit failed: {}",
                        slot_idx, e
                    );
                    last_err = Some(e);
                    failed += 1;
                } else {
                    submitted += 1;
                }
            }
            eprintln!(
                "[AokieRadio] SCO IN ring bootstrap: {}/{} slots submitted (all ContinueStream=FALSE, BTstack-parity), failed={}",
                submitted,
                total_slots,
                failed,
            );
            if buffers.in_pending == 0 {
                // Every slot rejected the submit — there's no in-flight
                // I/O at all, so surface the failure to the runtime.
                return Err(
                    last_err.unwrap_or_else(|| "SCO IN bootstrap accepted no slots".to_string())
                );
            }
        }

        // Drain any slots whose I/O has already completed. We give
        // the oldest slot a brief blocking window (5 ms) so this
        // function doesn't spin when the controller hasn't produced
        // a fresh frame yet, then poll the rest non-blocking.
        let mut data = Vec::new();
        let mut first = true;
        let ring_len = buffers.in_slots.len();
        // Per-iter diagnostics. Aggregated across calls and emitted
        // by `log_sco_rx_drain_diag` at most once every 2 s so we can
        // tell timeout-only ("URBs queued but kernel never signals")
        // from completed-but-empty-descriptors ("URBs complete but
        // controller delivered no payload") without flooding logs.
        let mut iter_timeouts = 0u32;
        let mut iter_completions = 0u32;
        let mut iter_completion_with_data = 0u32;
        let mut iter_completion_empty = 0u32;
        let mut iter_failed_overlapped = 0u32;
        let mut iter_io_incomplete = 0u32;
        // Bound how many SCO-IN slots we drain per call. On dongles whose
        // iso SCO-IN completes slots INSTANTLY (fast-fail, e.g. Win32 87 seen
        // on some Broadcom BCM20702 units), each re-submit completes right
        // away too, so this loop would never reach WAIT_TIMEOUT and would spin
        // forever — freezing the runtime's main loop, which then never drains
        // its control channel (a queued answer/hangup never gets sent) and the
        // call rings out / the link drops. Capping the per-call drain lets the
        // caller loop back to service control + HCI, then re-enter to drain the
        // next batch. 4× the ring is ample headroom for a real audio burst.
        let max_drain_per_call = ring_len.saturating_mul(4).max(16);
        let mut drained = 0usize;
        loop {
            // Skip past holes in the ring — slots whose re-submit
            // failed leave `in_use = false` until the ring fully
            // drains and we re-bootstrap. Without this, a hole at
            // `in_drain_idx` would short-circuit the whole drain loop
            // and mask data sitting in later slots' buffers.
            let mut scanned = 0;
            while scanned < ring_len && !buffers.in_slots[buffers.in_drain_idx].in_use {
                buffers.in_drain_idx = (buffers.in_drain_idx + 1) % ring_len;
                scanned += 1;
            }
            if scanned >= ring_len {
                break;
            }
            let drain_idx = buffers.in_drain_idx;

            let timeout = if first { 5 } else { 0 };
            let event = buffers.in_slots[drain_idx].event.0;
            let wait = unsafe { WaitForSingleObject(event, timeout) };
            match wait {
                WAIT_OBJECT_0 => {
                    first = false;
                    iter_completions += 1;
                    let mut transferred = 0u32;
                    let ok = unsafe {
                        WinUsb_GetOverlappedResult(
                            interface,
                            buffers.in_slots[drain_idx].overlapped.as_ref(),
                            &mut transferred,
                            0,
                        )
                    };
                    if ok == 0 {
                        let err = unsafe { GetLastError() };
                        if err == ERROR_IO_INCOMPLETE {
                            iter_io_incomplete += 1;
                            // OS says the I/O isn't finished even
                            // though the event signaled — leave the
                            // slot in flight and stop draining.
                            break;
                        }
                        iter_failed_overlapped += 1;
                        // Other failures (87 INVALID_PARAMETER, 31
                        // GEN_FAILURE, etc.) are usually per-slot
                        // hiccups. WinUSB sets the per-frame Length /
                        // Status fields on the descriptors
                        // independently of the overlapped status, so
                        // we fall through to extract whatever frames
                        // did arrive instead of dropping the slot's
                        // data on the floor. Logging is rate-limited
                        // to avoid drowning the console when error
                        // bursts hit.
                        log_sco_in_failure(drain_idx, err);
                    }

                    let slot_bytes = sco_in_slot_bytes(buffers.in_packet_size);
                    let slot_offset = drain_idx * slot_bytes;
                    let n_descriptors = buffers.in_slots[drain_idx].descriptors.len();
                    let data_len_before = data.len();
                    let mut sample_desc: [(u32, u32, u32); 4] = [(0, 0, 0); 4];
                    let sample_n = n_descriptors.min(sample_desc.len());
                    for i in 0..n_descriptors {
                        let (offset, len, status) = {
                            let desc = &buffers.in_slots[drain_idx].descriptors[i];
                            (desc.Offset as usize, desc.Length as usize, desc.Status)
                        };
                        if i < sample_n {
                            sample_desc[i] = (offset as u32, len as u32, status as u32);
                        }
                        // Skip empty or per-frame-error descriptors.
                        // Status 0 = USBD_STATUS_SUCCESS.
                        if len == 0 || status != 0 {
                            continue;
                        }
                        let frame_offset = slot_offset + offset;
                        if frame_offset + len > buffers.in_buffer.storage.len() {
                            return Err(format!(
                                "HCI SCO isoch descriptor exceeded buffer length: offset {} + len {} > {}",
                                frame_offset,
                                len,
                                buffers.in_buffer.storage.len()
                            ));
                        }
                        data.extend_from_slice(
                            &buffers.in_buffer.storage[frame_offset..frame_offset + len],
                        );
                    }
                    if data.len() == data_len_before {
                        iter_completion_empty += 1;
                        // Surface the actual descriptor (offset, length,
                        // Status) tuples on a periodic sample. Status is
                        // a USBD_STATUS value — high nibble 0xC means
                        // an error (e.g., 0xC0000030 NOT_ACCESSED,
                        // 0xC0010000 DEVICE_GONE). Length=0 with
                        // Status=0 means the controller deliberately
                        // sent a zero-length iso packet for that
                        // microframe, which is what a SCO IN endpoint
                        // normally does when it has no audio yet —
                        // but if every microframe of every URB is
                        // zero-length, the controller's iso route
                        // isn't actually carrying SCO traffic.
                        log_sco_rx_empty_descriptors(
                            drain_idx,
                            transferred,
                            &sample_desc[..sample_n],
                        );
                    } else {
                        iter_completion_with_data += 1;
                    }

                    buffers.in_slots[drain_idx].in_use = false;
                    buffers.in_pending -= 1;
                    buffers.in_drain_idx = (drain_idx + 1) % ring_len;
                    // Re-submit. Chain with ContinueStream=TRUE while
                    // there's still a predecessor in flight (matches
                    // BTstack's `usb_process_sco_in` →
                    // `usb_submit_sco_in_transfer_asap(idx, 1)`).
                    //
                    // If chaining fails (Win32 87 — usually the chain
                    // died because every prior slot already completed
                    // before we got here, or the ring temporarily
                    // emptied via re-submit failures), retry with
                    // FALSE to re-arm the stream. Without this fallback
                    // a single chain-death cascades into one failure
                    // per slot per iter — exactly the 87/31 flood we
                    // saw at ~90/sec when the host loop tick was
                    // longer than the chain duration.
                    let continue_stream = buffers.in_pending > 0;
                    if let Err(e) = submit_sco_in_slot(parent, buffers, drain_idx, continue_stream)
                    {
                        if continue_stream {
                            if let Err(e2) = submit_sco_in_slot(parent, buffers, drain_idx, false) {
                                eprintln!(
                                    "[AokieRadio] SCO in slot {} re-submit failed (chained {}, restart {})",
                                    drain_idx, e, e2
                                );
                            }
                        } else {
                            eprintln!(
                                "[AokieRadio] SCO in slot {} re-submit failed: {}",
                                drain_idx, e
                            );
                        }
                    }
                    drained += 1;
                    if drained >= max_drain_per_call {
                        // Bound reached — yield to the caller's main loop so it
                        // can drain control/HCI; the next read_sco resumes here.
                        break;
                    }
                }
                WAIT_TIMEOUT => {
                    iter_timeouts += 1;
                    break;
                }
                _ => {
                    let err = unsafe { GetLastError() };
                    return Err(format!(
                        "WaitForSingleObject(SCO in slot {}) failed: Win32 error {}",
                        drain_idx, err
                    ));
                }
            }
        }
        log_sco_rx_drain_diag(
            iter_timeouts,
            iter_completions,
            iter_completion_with_data,
            iter_completion_empty,
            iter_failed_overlapped,
            iter_io_incomplete,
            buffers.in_pending,
            buffers.in_drain_idx,
        );
        Ok(data)
    }

    fn write_sco_isoch(&mut self, packet: &[u8]) -> Result<(), String> {
        let interface = self.pipe_handle(InterfaceSlot::Associated)?;
        // Control handle for the iso-frame-clock prime BTstack issues
        // before every WriteIsochPipeAsap (`hci_transport_h2_winusb.c:1428`).
        let parent = self.pipe_handle(InterfaceSlot::Control)?;
        // The REAL file handle — CancelIoEx wants this, never the opaque
        // WinUSB interface handle (audit AK-01).
        let device = self._device.0;
        let Some(buffers) = self.sco_isoch_buffers.as_mut() else {
            return Err("HCI SCO isochronous buffers are not registered".to_string());
        };
        if packet.len() > SCO_OUT_MAX_HCI_PACKET_BYTES {
            return Err(format!(
                "HCI SCO packet is too large for ring slot: {} > {}",
                packet.len(),
                SCO_OUT_MAX_HCI_PACKET_BYTES
            ));
        }

        // Reap any completions opportunistically — non-blocking, just
        // to keep the ring as empty as possible before we add to it.
        drain_sco_out_completions(buffers, device, interface, false)?;

        // If the ring is still full, block on the oldest in-flight
        // slot. This is the natural backpressure that paces our
        // submissions to the controller's SCO link rate without
        // having to track Number_Of_Completed_Packets.
        if buffers.out_pending >= buffers.out_slots.len() {
            drain_sco_out_completions(buffers, device, interface, true)?;
        }

        let slot_idx = buffers.out_write_idx;
        if buffers.out_slots[slot_idx].in_use {
            return Err(format!(
                "SCO out ring slot {} still in use after drain (pending {})",
                slot_idx, buffers.out_pending
            ));
        }

        let offset = slot_idx * buffers.out_slot_stride;
        debug_assert_eq!(
            offset % buffers.out_packet_size as usize,
            0,
            "SCO out offset {} must be a multiple of active MPS={}",
            offset,
            buffers.out_packet_size
        );
        buffers.out_buffer.storage[offset..offset + packet.len()].copy_from_slice(packet);

        let slot = &mut buffers.out_slots[slot_idx];
        // Reinit overlapped: the OS only writes Internal/InternalHigh
        // during the I/O, but zeroing in case we re-use a slot whose
        // previous I/O finished or was cancelled.
        *slot.overlapped = unsafe { zeroed::<OVERLAPPED>() };
        slot.overlapped.hEvent = slot.event.0;
        unsafe { ResetEvent(slot.event.0) };

        // ContinueStream: TRUE while the ring still has predecessors
        // in flight, FALSE when the ring is empty (first write of a
        // freshly-opened SCO link, or the first write after the ring
        // drained to zero between TTS bursts). With single-pending
        // sync writes — or after the ring naturally drains during an
        // idle gap — chaining onto nothing fails with Win32 error 87
        // (ERROR_INVALID_PARAMETER). Tying the flag directly to
        // `out_pending` makes this self-correcting: every empty ring
        // restarts the stream, every populated ring chains.
        let mut continue_stream: i32 = if buffers.out_pending > 0 { 1 } else { 0 };

        unsafe { prime_iso_frame_clock(parent) };
        let mut submit_ok = unsafe {
            WinUsb_WriteIsochPipeAsap(
                buffers.out_buffer.handle.as_ptr(),
                offset as u32,
                packet.len() as u32,
                continue_stream,
                slot.overlapped.as_ref() as *const _,
            )
        };

        if submit_ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                // If we tried to chain onto a stream that has died
                // (every previous write completed before we got here —
                // happens between TTS bursts when the ring naturally
                // drains, or when a long iter tick lets the ring run
                // empty), Win32 returns 87. Re-arm the stream by
                // retrying with ContinueStream=FALSE.
                if continue_stream != 0 && err == 87 {
                    SCO_TX_STREAM_RESETS.fetch_add(1, AtomicOrdering::Relaxed);
                    *slot.overlapped = unsafe { zeroed::<OVERLAPPED>() };
                    slot.overlapped.hEvent = slot.event.0;
                    unsafe { ResetEvent(slot.event.0) };
                    continue_stream = 0;
                    unsafe { prime_iso_frame_clock(parent) };
                    submit_ok = unsafe {
                        WinUsb_WriteIsochPipeAsap(
                            buffers.out_buffer.handle.as_ptr(),
                            offset as u32,
                            packet.len() as u32,
                            continue_stream,
                            slot.overlapped.as_ref() as *const _,
                        )
                    };
                    if submit_ok == 0 {
                        let err2 = unsafe { GetLastError() };
                        if err2 != ERROR_IO_PENDING {
                            return Err(format!(
                                "WinUsb_WriteIsochPipeAsap(HCI SCO out) failed (chained 87, restart {})",
                                err2
                            ));
                        }
                    }
                } else {
                    return Err(format!(
                        "WinUsb_WriteIsochPipeAsap(HCI SCO out) failed: Win32 error {}",
                        err
                    ));
                }
            }
        }

        slot.in_use = true;
        buffers.out_pending += 1;
        buffers.out_write_idx = (slot_idx + 1) % buffers.out_slots.len();
        Ok(())
    }

    fn pipe_handle(&self, slot: InterfaceSlot) -> Result<WINUSB_INTERFACE_HANDLE, String> {
        match slot {
            InterfaceSlot::Control => Ok(self.control.0),
            InterfaceSlot::Associated => self
                .associated
                .as_ref()
                .map(|interface| interface.0)
                .ok_or_else(|| "WinUSB associated interface is not open".to_string()),
        }
    }
}

impl PacketDump {
    fn from_env() -> Self {
        // AOKIE_RADIO_DUMP / AOKIE_HCI_DUMP write a high-volume hex dump
        // of every HCI/ACL/SCO transfer. The framing of message bodies
        // and transcripts is visible there, so it doesn't belong on a
        // packaged install — release builds refuse the env var entirely.
        // The R8 review flagged this as a release-build hazard parallel
        // to AOKIE_DUMP_RX_WAV (caller audio dumps).
        let enabled = if cfg!(debug_assertions) {
            std::env::var_os("AOKIE_RADIO_DUMP").is_some()
                || std::env::var_os("AOKIE_HCI_DUMP").is_some()
        } else {
            false
        };
        Self { enabled }
    }

    fn log(&self, direction: &str, packet: &[u8]) {
        if self.enabled {
            eprintln!("[AokieRadio] {} {}", direction, hex_bytes(packet));
        }
    }
}

impl Drop for WinUsbIsochBuffer {
    fn drop(&mut self) {
        unsafe {
            WinUsb_UnregisterIsochBuffer(self.handle.as_ptr());
        }
    }
}

fn interface_handle(
    control: &WinUsbInterface,
    associated: Option<&WinUsbInterface>,
    slot: InterfaceSlot,
) -> Result<WINUSB_INTERFACE_HANDLE, String> {
    match slot {
        InterfaceSlot::Control => Ok(control.0),
        InterfaceSlot::Associated => associated
            .map(|interface| interface.0)
            .ok_or_else(|| "WinUSB associated interface is not open".to_string()),
    }
}

fn classify_transport_pipes(
    control: &InterfaceDiagnostics,
    associated: Option<&InterfaceDiagnostics>,
) -> TransportPipeSet {
    let mut out = TransportPipeSet::default();
    add_transport_pipes(&mut out, InterfaceSlot::Control, &control.pipes);
    if let Some(associated) = associated {
        add_transport_pipes(&mut out, InterfaceSlot::Associated, &associated.pipes);
    }
    out
}

fn add_transport_pipes(out: &mut TransportPipeSet, slot: InterfaceSlot, pipes: &[PipeInfo]) {
    for &info in pipes {
        let endpoint = PipeEndpoint { info, slot };
        match (info.kind, info.direction) {
            (PipeKind::Interrupt, PipeDirection::In) if out.event_in.is_none() => {
                out.event_in = Some(endpoint)
            }
            (PipeKind::Bulk, PipeDirection::In) if out.acl_in.is_none() => {
                out.acl_in = Some(endpoint)
            }
            (PipeKind::Bulk, PipeDirection::Out) if out.acl_out.is_none() => {
                out.acl_out = Some(endpoint)
            }
            (PipeKind::Isochronous, PipeDirection::In) if out.sco_in.is_none() => {
                out.sco_in = Some(endpoint)
            }
            (PipeKind::Isochronous, PipeDirection::Out) if out.sco_out.is_none() => {
                out.sco_out = Some(endpoint)
            }
            _ => {}
        }
    }
}

fn sco_alt_setting_for_voice(voice_setting: u16, connection_count: usize) -> Option<u8> {
    // BTstack `hci_transport_h2_winusb.c:907-913` picks the alt setting
    // purely from voice_setting bit 5 (input sample size) and the
    // connection count — there is no transparent → alt 6 special case.
    // mSBC over USB rides alt 1 (MPS=9) with 24-byte HCI SCO payloads
    // (see `AOKIE_SCO_USB_PAYLOAD_BYTES`). Routing transparent voice to
    // a separate alt-6 pipe was an over-read of the Core spec USB
    // Transport Layer table: BTstack's `hci.c` keeps the same path for
    // CVSD and mSBC and that is the configuration proven to work on the
    // Broadcom 21ec dongle in our worktree comparison.
    let index = connection_count.checked_sub(1)?;
    if voice_setting & 0x0020 != 0 {
        SCO_ALT_SETTINGS_16_BIT.get(index).copied()
    } else {
        SCO_ALT_SETTINGS_8_BIT.get(index).copied()
    }
}

fn sco_isoch_buffer_len(max_packet_size: u16) -> usize {
    max_packet_size as usize * SCO_IN_PACKETS_PER_SLOT * SCO_IN_RING_SLOTS
}

fn sco_in_slot_bytes(max_packet_size: u16) -> usize {
    max_packet_size as usize * SCO_IN_PACKETS_PER_SLOT
}

fn sco_transport_config(
    alternate_setting: u8,
    sco_in: Option<PipeEndpoint>,
    sco_out: Option<PipeEndpoint>,
    isoch_buffers_registered: bool,
) -> ScoTransportConfig {
    let max_packet_size = sco_in
        .or(sco_out)
        .map(|endpoint| endpoint.info.max_packet_size);
    ScoTransportConfig {
        alternate_setting,
        in_pipe_id: sco_in.map(|endpoint| endpoint.info.id),
        out_pipe_id: sco_out.map(|endpoint| endpoint.info.id),
        max_packet_size,
        isoch_buffer_len: max_packet_size.map(sco_isoch_buffer_len),
        isoch_buffers_registered,
    }
}

fn hex_bytes(packet: &[u8]) -> String {
    let mut out = String::with_capacity(packet.len().saturating_mul(3).saturating_sub(1));
    for (index, byte) in packet.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn pipe_kind(kind: i32) -> PipeKind {
    if kind == UsbdPipeTypeBulk {
        PipeKind::Bulk
    } else if kind == UsbdPipeTypeInterrupt {
        PipeKind::Interrupt
    } else if kind == UsbdPipeTypeIsochronous {
        PipeKind::Isochronous
    } else if kind == 0 {
        PipeKind::Control
    } else {
        PipeKind::Unknown(kind)
    }
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

struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

struct DeviceHandle(HANDLE);

struct EventHandle(HANDLE);

impl DeviceHandle {
    unsafe fn open(path: &str) -> Result<Self, String> {
        let path_w = wide_null(path);
        let handle = CreateFileW(
            path_w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
            null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "CreateFileW failed for WinUSB interface: Win32 error {}",
                GetLastError()
            ));
        }
        Ok(Self(handle))
    }
}

impl EventHandle {
    unsafe fn manual_reset() -> Result<Self, String> {
        let handle = CreateEventW(null(), 1, 0, null());
        if handle.is_null() {
            return Err(format!(
                "CreateEventW(overlapped) failed: Win32 error {}",
                GetLastError()
            ));
        }
        Ok(Self(handle))
    }
}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct WinUsbInterface(WINUSB_INTERFACE_HANDLE);

impl WinUsbInterface {
    unsafe fn initialize(device: HANDLE) -> Result<Self, String> {
        let mut handle = null_mut();
        if WinUsb_Initialize(device, &mut handle) == 0 {
            return Err(format!(
                "WinUsb_Initialize failed: Win32 error {}",
                GetLastError()
            ));
        }
        Ok(Self(handle))
    }

    unsafe fn associated(parent: WINUSB_INTERFACE_HANDLE, index: u8) -> Result<Self, String> {
        let mut handle = null_mut();
        if WinUsb_GetAssociatedInterface(parent, index, &mut handle) == 0 {
            return Err(format!(
                "WinUsb_GetAssociatedInterface({}) failed: Win32 error {}",
                index,
                GetLastError()
            ));
        }
        Ok(Self(handle))
    }
}

impl Drop for WinUsbInterface {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WinUsb_Free(self.0);
            }
        }
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_standard_hci_pipes() {
        let pipes = [
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
                max_packet_size: 49,
                interval: 1,
                alternate_setting: 1,
            },
        ];
        let classified = classify_hci_pipes(&pipes);
        assert_eq!(classified.event_in.unwrap().id, 0x81);
        assert_eq!(classified.acl_in.unwrap().id, 0x82);
        assert_eq!(classified.acl_out.unwrap().id, 0x02);
        assert_eq!(classified.sco_in.unwrap().id, 0x83);
        assert_eq!(classified.sco_out, None);
    }

    #[test]
    fn transport_pipe_classification_preserves_interface_slots() {
        let control = InterfaceDiagnostics {
            device_path: "test".to_string(),
            interface_number: 0,
            alternate_setting: 0,
            pipes: vec![
                PipeInfo {
                    id: 0x81,
                    kind: PipeKind::Interrupt,
                    direction: PipeDirection::In,
                    max_packet_size: 16,
                    interval: 1,
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
            ],
            classified: HciPipes::default(),
        };
        let associated = InterfaceDiagnostics {
            device_path: "test".to_string(),
            interface_number: 1,
            alternate_setting: 0,
            pipes: vec![PipeInfo {
                id: 0x83,
                kind: PipeKind::Isochronous,
                direction: PipeDirection::In,
                max_packet_size: 49,
                interval: 1,
                alternate_setting: 1,
            }],
            classified: HciPipes::default(),
        };

        let pipes = classify_transport_pipes(&control, Some(&associated));
        assert_eq!(pipes.event_in.unwrap().slot, InterfaceSlot::Control);
        assert_eq!(pipes.acl_out.unwrap().slot, InterfaceSlot::Control);
        assert_eq!(pipes.sco_in.unwrap().slot, InterfaceSlot::Associated);
    }

    #[test]
    fn hci_transport_requires_event_and_acl_pipes() {
        let complete = HciPipes {
            event_in: Some(PipeInfo {
                id: 0x81,
                kind: PipeKind::Interrupt,
                direction: PipeDirection::In,
                max_packet_size: 16,
                interval: 1,
                alternate_setting: 0,
            }),
            acl_in: Some(PipeInfo {
                id: 0x82,
                kind: PipeKind::Bulk,
                direction: PipeDirection::In,
                max_packet_size: 64,
                interval: 0,
                alternate_setting: 0,
            }),
            acl_out: Some(PipeInfo {
                id: 0x02,
                kind: PipeKind::Bulk,
                direction: PipeDirection::Out,
                max_packet_size: 64,
                interval: 0,
                alternate_setting: 0,
            }),
            sco_in: None,
            sco_out: None,
        };
        assert!(hci_transport_pipes_available(&complete));

        let mut missing_acl_out = complete;
        missing_acl_out.acl_out = None;
        assert!(!hci_transport_pipes_available(&missing_acl_out));
        assert!(!hci_transport_pipes_available(&HciPipes::default()));
    }

    #[test]
    fn sco_alt_setting_matches_btstack_single_connection_mapping() {
        assert_eq!(sco_alt_setting_for_voice(0x0040, 1), Some(1));
        assert_eq!(sco_alt_setting_for_voice(0x0060, 1), Some(2));
        assert_eq!(sco_alt_setting_for_voice(0x0040, 3), Some(3));
        assert_eq!(sco_alt_setting_for_voice(0x0060, 3), Some(5));
        assert_eq!(sco_alt_setting_for_voice(0x0060, 4), None);
        assert_eq!(sco_alt_setting_for_voice(0x0060, 0), None);
    }

    #[test]
    fn sco_alt_setting_for_msbc_matches_btstack_table() {
        // 0x0043 = AOKIE_VOICE_SETTING_TRANSPARENT. Bit 5 clear → 8-bit
        // input table; one connection → alt 1 (MPS=9). This is the
        // BTstack-compatible mSBC routing.
        assert_eq!(sco_alt_setting_for_voice(0x0043, 1), Some(1));
        // 0x0063 = transparent + 16-bit input. Bit 5 set picks the
        // 16-bit table; one connection → alt 2. Kept here for anyone
        // who hard-codes the older constant value.
        assert_eq!(sco_alt_setting_for_voice(0x0063, 1), Some(2));
        // Multi-connection transparent: bit 5 still picks the 16-bit
        // table, second connection → alt 4.
        assert_eq!(sco_alt_setting_for_voice(0x0063, 2), Some(4));
        // CVSD-16-bit unchanged.
        assert_eq!(sco_alt_setting_for_voice(0x0060, 1), Some(2));
    }

    #[test]
    fn sco_isoch_buffer_len_matches_btstack_ring_shape() {
        // 16 ring slots × 3 isoch packets per slot × max packet size.
        assert_eq!(sco_isoch_buffer_len(9), 9 * 3 * 16);
        assert_eq!(sco_isoch_buffer_len(49), 49 * 3 * 16);
    }

    #[test]
    fn sco_transport_config_reports_pipes_and_buffer_shape() {
        let in_endpoint = PipeEndpoint {
            info: PipeInfo {
                id: 0x83,
                kind: PipeKind::Isochronous,
                direction: PipeDirection::In,
                max_packet_size: 49,
                interval: 1,
                alternate_setting: 2,
            },
            slot: InterfaceSlot::Associated,
        };
        let out_endpoint = PipeEndpoint {
            info: PipeInfo {
                id: 0x03,
                kind: PipeKind::Isochronous,
                direction: PipeDirection::Out,
                max_packet_size: 49,
                interval: 1,
                alternate_setting: 2,
            },
            slot: InterfaceSlot::Associated,
        };

        assert_eq!(
            sco_transport_config(2, Some(in_endpoint), Some(out_endpoint), true),
            ScoTransportConfig {
                alternate_setting: 2,
                in_pipe_id: Some(0x83),
                out_pipe_id: Some(0x03),
                max_packet_size: Some(49),
                isoch_buffer_len: Some(49 * 3 * 16),
                isoch_buffers_registered: true,
            }
        );
    }

    #[test]
    fn hex_bytes_formats_packet_dump() {
        assert_eq!(hex_bytes(&[0x01, 0xab, 0x00]), "01 ab 00");
        assert_eq!(hex_bytes(&[]), "");
    }

    #[test]
    fn pipe_kind_maps_winusb_values() {
        assert_eq!(pipe_kind(UsbdPipeTypeBulk), PipeKind::Bulk);
        assert_eq!(pipe_kind(UsbdPipeTypeInterrupt), PipeKind::Interrupt);
        assert_eq!(pipe_kind(UsbdPipeTypeIsochronous), PipeKind::Isochronous);
        assert_eq!(pipe_kind(123), PipeKind::Unknown(123));
    }

    #[test]
    fn pipe_timeouts_only_apply_to_interrupt_and_bulk_pipes() {
        assert!(pipe_timeout_supported(PipeKind::Interrupt));
        assert!(pipe_timeout_supported(PipeKind::Bulk));
        assert!(!pipe_timeout_supported(PipeKind::Isochronous));
        assert!(!pipe_timeout_supported(PipeKind::Control));
        assert!(!pipe_timeout_supported(PipeKind::Unknown(123)));
    }
}
