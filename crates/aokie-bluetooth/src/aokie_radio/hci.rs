#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Command = 0x01,
    AclData = 0x02,
    ScoData = 0x03,
    Event = 0x04,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HciPacket<'a> {
    Command {
        opcode: u16,
        params: &'a [u8],
    },
    AclData {
        handle_pb_bc: u16,
        payload: &'a [u8],
    },
    ScoData {
        handle_status: u16,
        payload: &'a [u8],
    },
    Event {
        event_code: u8,
        params: &'a [u8],
    },
}

pub const OPCODE_RESET: u16 = 0x0c03;
pub const OPCODE_CREATE_CONNECTION: u16 = 0x0405;
pub const OPCODE_DISCONNECT: u16 = 0x0406;
pub const OPCODE_REMOTE_NAME_REQUEST: u16 = 0x0419;
pub const OPCODE_ACCEPT_CONNECTION_REQUEST: u16 = 0x0409;
pub const OPCODE_REJECT_CONNECTION_REQUEST: u16 = 0x040a;
pub const OPCODE_ACCEPT_SYNCHRONOUS_CONNECTION_REQUEST: u16 = 0x0429;
pub const OPCODE_LINK_KEY_REQUEST_REPLY: u16 = 0x040b;
pub const OPCODE_LINK_KEY_REQUEST_NEGATIVE_REPLY: u16 = 0x040c;
pub const OPCODE_PIN_CODE_REQUEST_REPLY: u16 = 0x040d;
pub const OPCODE_PIN_CODE_REQUEST_NEGATIVE_REPLY: u16 = 0x040e;
pub const OPCODE_IO_CAPABILITY_REQUEST_REPLY: u16 = 0x042b;
pub const OPCODE_USER_CONFIRMATION_REQUEST_REPLY: u16 = 0x042c;
pub const OPCODE_USER_CONFIRMATION_REQUEST_NEGATIVE_REPLY: u16 = 0x042d;
pub const OPCODE_IO_CAPABILITY_REQUEST_NEGATIVE_REPLY: u16 = 0x0434;
pub const OPCODE_SWITCH_ROLE: u16 = 0x080b;
pub const OPCODE_WRITE_DEFAULT_LINK_POLICY_SETTINGS: u16 = 0x080f;
pub const OPCODE_SET_EVENT_MASK: u16 = 0x0c01;
pub const OPCODE_WRITE_LOCAL_NAME: u16 = 0x0c13;
pub const OPCODE_WRITE_PAGE_TIMEOUT: u16 = 0x0c18;
pub const OPCODE_WRITE_SCAN_ENABLE: u16 = 0x0c1a;
pub const OPCODE_WRITE_CLASS_OF_DEVICE: u16 = 0x0c24;
pub const OPCODE_WRITE_VOICE_SETTING: u16 = 0x0c26;
pub const OPCODE_WRITE_EXTENDED_INQUIRY_RESPONSE: u16 = 0x0c52;
pub const OPCODE_WRITE_SIMPLE_PAIRING_MODE: u16 = 0x0c56;
pub const OPCODE_READ_LOCAL_VERSION_INFORMATION: u16 = 0x1001;
pub const OPCODE_READ_LOCAL_SUPPORTED_FEATURES: u16 = 0x1003;
pub const OPCODE_READ_BUFFER_SIZE: u16 = 0x1005;
pub const OPCODE_READ_BD_ADDR: u16 = 0x1009;

/// Broadcom/Cypress vendor: route SCO data via HCI iso transport (or PCM).
/// `HCI_OPCODE(OGF=0x3F, OCF=0x1C)` per BTStack `hci_cmd.h:348`. Sent at
/// init time on Broadcom controllers; without it BCM20702A0 silently
/// routes SCO to its (unconnected) PCM/I2S pins and HCI iso carries
/// zero bytes both ways.
pub const OPCODE_BCM_WRITE_SCO_PCM_INT: u16 = 0xfc1c;

/// Bluetooth SIG company ID for Broadcom Corporation. Reported in the
/// `manufacturer_name` field of `Read_Local_Version_Information`.
pub const BLUETOOTH_COMPANY_ID_BROADCOM: u16 = 0x000f;

pub const LOCAL_NAME_PARAM_LEN: usize = 248;
pub const LINK_KEY_LEN: usize = 16;
pub const PIN_CODE_PARAM_LEN: usize = 16;
/// PAIR-001: the production runtime advertises DisplayYesNo — the FormLogic
/// Desktop UI is the display, so SSP resolves to NUMERIC COMPARISON (both
/// sides show the same 6-digit code and a human confirms on each).
pub const SSP_IO_CAPABILITY_DISPLAY_YES_NO: u8 = 0x01;
pub const SSP_IO_CAPABILITY_NO_INPUT_NO_OUTPUT: u8 = 0x03;
pub const SSP_OOB_DATA_NOT_PRESENT: u8 = 0x00;
pub const SSP_AUTHREQ_MITM_NOT_REQUIRED_GENERAL_BONDING: u8 = 0x04;
/// PAIR-001: MITM protection required — pairs with DisplayYesNo above so a
/// silent just-works bond can't be completed by a nearby stranger racing the
/// intended phone during an open pairing window.
pub const SSP_AUTHREQ_MITM_REQUIRED_GENERAL_BONDING: u8 = 0x05;
pub const ACCEPT_ROLE_REMAIN_SLAVE: u8 = 0x01;
pub const LINK_TYPE_SCO: u8 = 0x00;
pub const LINK_TYPE_ACL: u8 = 0x01;
pub const LINK_TYPE_ESCO: u8 = 0x02;
pub const SCO_RETRANSMISSION_EFFORT_POWER_OPTIMIZED: u8 = 0x00;
pub const SCO_RETRANSMISSION_EFFORT_DONT_CARE: u8 = 0xff;
pub const SCO_PACKET_TYPE_HV1: u16 = 0x0001;
pub const SCO_PACKET_TYPE_HV3: u16 = 0x0004;
pub const SCO_PACKET_TYPE_EV3: u16 = 0x0008;
pub const SCO_PACKET_TYPE_2EV3: u16 = 0x0040;
pub const SCO_PACKET_TYPES_COMMAND_FLIP_MASK: u16 = 0x03c0;
pub const SCO_PACKET_TYPES_HFP_CVSD_SCO_COMMAND: u16 =
    (SCO_PACKET_TYPE_HV1 | SCO_PACKET_TYPE_HV3) ^ SCO_PACKET_TYPES_COMMAND_FLIP_MASK;
pub const SCO_PACKET_TYPES_HFP_CVSD_ESCO_COMMAND: u16 =
    (SCO_PACKET_TYPE_EV3 | SCO_PACKET_TYPE_2EV3) ^ SCO_PACKET_TYPES_COMMAND_FLIP_MASK;

pub const EVENT_CONNECTION_COMPLETE: u8 = 0x03;
pub const EVENT_CONNECTION_REQUEST: u8 = 0x04;
pub const EVENT_DISCONNECTION_COMPLETE: u8 = 0x05;
pub const EVENT_REMOTE_NAME_REQUEST_COMPLETE: u8 = 0x07;
pub const EVENT_COMMAND_COMPLETE: u8 = 0x0e;
pub const EVENT_COMMAND_STATUS: u8 = 0x0f;
pub const EVENT_PIN_CODE_REQUEST: u8 = 0x16;
pub const EVENT_LINK_KEY_REQUEST: u8 = 0x17;
pub const EVENT_LINK_KEY_NOTIFICATION: u8 = 0x18;
pub const EVENT_SYNCHRONOUS_CONNECTION_COMPLETE: u8 = 0x2c;
pub const EVENT_IO_CAPABILITY_REQUEST: u8 = 0x31;
pub const EVENT_USER_CONFIRMATION_REQUEST: u8 = 0x33;
pub const EVENT_SIMPLE_PAIRING_COMPLETE: u8 = 0x36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalVersion {
    pub hci_version: u8,
    pub hci_revision: u16,
    pub lmp_pal_version: u8,
    pub manufacturer_name: u16,
    pub lmp_pal_subversion: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferSize {
    pub acl_data_packet_length: u16,
    pub sco_data_packet_length: u8,
    pub total_num_acl_data_packets: u16,
    pub total_num_sco_data_packets: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HciEvent {
    CommandComplete {
        num_hci_command_packets: u8,
        opcode: u16,
        return_params: Vec<u8>,
    },
    CommandStatus {
        status: u8,
        num_hci_command_packets: u8,
        opcode: u16,
    },
    ConnectionComplete {
        status: u8,
        connection_handle: u16,
        address: String,
        link_type: u8,
        encryption_enabled: u8,
    },
    ConnectionRequest {
        address: String,
        class_of_device: u32,
        link_type: u8,
    },
    DisconnectionComplete {
        status: u8,
        connection_handle: u16,
        reason: u8,
    },
    RemoteNameRequestComplete {
        status: u8,
        address: String,
        name: String,
    },
    PinCodeRequest {
        address: String,
    },
    LinkKeyRequest {
        address: String,
    },
    LinkKeyNotification {
        address: String,
        link_key: [u8; 16],
        key_type: u8,
    },
    SynchronousConnectionComplete {
        status: u8,
        connection_handle: u16,
        address: String,
        link_type: u8,
        transmission_interval: u8,
        retransmission_window: u8,
        rx_packet_length: u16,
        tx_packet_length: u16,
        air_mode: u8,
    },
    IoCapabilityRequest {
        address: String,
    },
    UserConfirmationRequest {
        address: String,
        numeric_value: u32,
    },
    SimplePairingComplete {
        status: u8,
        address: String,
    },
    Unknown {
        event_code: u8,
        params: Vec<u8>,
    },
}

impl HciEvent {
    pub fn event_code(&self) -> u8 {
        match self {
            Self::CommandComplete { .. } => EVENT_COMMAND_COMPLETE,
            Self::CommandStatus { .. } => EVENT_COMMAND_STATUS,
            Self::ConnectionComplete { .. } => EVENT_CONNECTION_COMPLETE,
            Self::ConnectionRequest { .. } => EVENT_CONNECTION_REQUEST,
            Self::DisconnectionComplete { .. } => EVENT_DISCONNECTION_COMPLETE,
            Self::RemoteNameRequestComplete { .. } => EVENT_REMOTE_NAME_REQUEST_COMPLETE,
            Self::PinCodeRequest { .. } => EVENT_PIN_CODE_REQUEST,
            Self::LinkKeyRequest { .. } => EVENT_LINK_KEY_REQUEST,
            Self::LinkKeyNotification { .. } => EVENT_LINK_KEY_NOTIFICATION,
            Self::SynchronousConnectionComplete { .. } => EVENT_SYNCHRONOUS_CONNECTION_COMPLETE,
            Self::IoCapabilityRequest { .. } => EVENT_IO_CAPABILITY_REQUEST,
            Self::UserConfirmationRequest { .. } => EVENT_USER_CONFIRMATION_REQUEST,
            Self::SimplePairingComplete { .. } => EVENT_SIMPLE_PAIRING_COMPLETE,
            Self::Unknown { event_code, .. } => *event_code,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::CommandComplete { .. } => "Command Complete",
            Self::CommandStatus { .. } => "Command Status",
            Self::ConnectionComplete { .. } => "Connection Complete",
            Self::ConnectionRequest { .. } => "Connection Request",
            Self::DisconnectionComplete { .. } => "Disconnection Complete",
            Self::RemoteNameRequestComplete { .. } => "Remote Name Request Complete",
            Self::PinCodeRequest { .. } => "PIN Code Request",
            Self::LinkKeyRequest { .. } => "Link Key Request",
            Self::LinkKeyNotification { .. } => "Link Key Notification",
            Self::SynchronousConnectionComplete { .. } => "Synchronous Connection Complete",
            Self::IoCapabilityRequest { .. } => "IO Capability Request",
            Self::UserConfirmationRequest { .. } => "User Confirmation Request",
            Self::SimplePairingComplete { .. } => "Simple Pairing Complete",
            Self::Unknown { .. } => "Unknown",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::CommandComplete {
                opcode,
                return_params,
                ..
            } => {
                format!(
                    "opcode 0x{:04x}, {} return bytes",
                    opcode,
                    return_params.len()
                )
            }
            Self::CommandStatus { status, opcode, .. } => {
                format!("opcode 0x{:04x}, status 0x{:02x}", opcode, status)
            }
            Self::ConnectionComplete {
                status,
                connection_handle,
                address,
                link_type,
                ..
            } => {
                format!(
                    "{} handle 0x{:04x}, link {}, status 0x{:02x}",
                    address, connection_handle, link_type, status
                )
            }
            Self::DisconnectionComplete {
                status,
                connection_handle,
                reason,
            } => {
                format!(
                    "handle 0x{:04x}, reason 0x{:02x}, status 0x{:02x}",
                    connection_handle, reason, status
                )
            }
            Self::ConnectionRequest {
                address,
                class_of_device,
                link_type,
            } => {
                format!(
                    "{} class 0x{:06x}, link {}",
                    address, class_of_device, link_type
                )
            }
            Self::RemoteNameRequestComplete {
                status,
                address,
                name,
            } => {
                format!("{} '{}', status 0x{:02x}", address, name, status)
            }
            Self::PinCodeRequest { address }
            | Self::LinkKeyRequest { address }
            | Self::IoCapabilityRequest { address } => address.clone(),
            Self::LinkKeyNotification {
                address, key_type, ..
            } => {
                format!("{} key type 0x{:02x}", address, key_type)
            }
            Self::SynchronousConnectionComplete {
                status,
                connection_handle,
                address,
                link_type,
                rx_packet_length,
                tx_packet_length,
                air_mode,
                ..
            } => {
                format!(
                    "{} handle 0x{:04x}, link {}, rx {}, tx {}, air 0x{:02x}, status 0x{:02x}",
                    address,
                    connection_handle,
                    link_type,
                    rx_packet_length,
                    tx_packet_length,
                    air_mode,
                    status
                )
            }
            Self::UserConfirmationRequest {
                address,
                numeric_value,
            } => {
                format!("{} numeric value {}", address, numeric_value)
            }
            Self::SimplePairingComplete { status, address } => {
                format!("{} status 0x{:02x}", address, status)
            }
            Self::Unknown { event_code, params } => {
                format!(
                    "event 0x{:02x}, {} parameter bytes",
                    event_code,
                    params.len()
                )
            }
        }
    }
}

impl PacketType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::Command),
            0x02 => Some(Self::AclData),
            0x03 => Some(Self::ScoData),
            0x04 => Some(Self::Event),
            _ => None,
        }
    }
}

pub fn reset_command() -> [u8; 3] {
    // Opcode 0x0C03: OGF Controller/Baseband, OCF Reset.
    command_no_params(OPCODE_RESET)
}

pub fn read_bd_addr_command() -> [u8; 3] {
    // Opcode 0x1009: OGF Informational Parameters, OCF Read BD_ADDR.
    command_no_params(OPCODE_READ_BD_ADDR)
}

pub fn read_local_version_information_command() -> [u8; 3] {
    command_no_params(OPCODE_READ_LOCAL_VERSION_INFORMATION)
}

pub fn read_local_supported_features_command() -> [u8; 3] {
    command_no_params(OPCODE_READ_LOCAL_SUPPORTED_FEATURES)
}

pub fn read_buffer_size_command() -> [u8; 3] {
    command_no_params(OPCODE_READ_BUFFER_SIZE)
}

pub fn set_event_mask_command(mask: u64) -> [u8; 11] {
    let mut out = [0u8; 11];
    out[0..2].copy_from_slice(&OPCODE_SET_EVENT_MASK.to_le_bytes());
    out[2] = 8;
    out[3..11].copy_from_slice(&mask.to_le_bytes());
    out
}

pub fn link_key_request_reply_command(
    address: &str,
    link_key: &[u8; LINK_KEY_LEN],
) -> Result<[u8; 25], String> {
    let mut out = [0u8; 25];
    out[0..2].copy_from_slice(&OPCODE_LINK_KEY_REQUEST_REPLY.to_le_bytes());
    out[2] = 22;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9..25].copy_from_slice(link_key);
    Ok(out)
}

/// Build HCI_Create_Connection (BT spec v5.4 vol 4 part E §7.1.5).
/// Issued by us (the host) to actively page a paired peer device. The
/// controller responds first with a Command Status, then later with a
/// Connection Complete event carrying status + handle once the peer
/// responds (or the page times out after WRITE_PAGE_TIMEOUT slots).
///
/// Defaults baked in by `create_connection_command_default`:
///   - packet_type = 0xCC18 (DM1 + DH1 + DM3 + DH3 + DM5 + DH5 — the
///     standard "all BR rates" mask Bluedroid uses)
///   - page_scan_repetition_mode = R1 (most phones advertise R1)
///   - clock_offset = 0 (unknown — controller falls back to inquiry
///     result if any, or pages from scratch)
///   - allow_role_switch = 1 (let the peer become master if it prefers)
pub fn create_connection_command(
    address: &str,
    packet_type: u16,
    page_scan_repetition_mode: u8,
    clock_offset: u16,
    allow_role_switch: u8,
) -> Result<[u8; 16], String> {
    let mut out = [0u8; 16];
    out[0..2].copy_from_slice(&OPCODE_CREATE_CONNECTION.to_le_bytes());
    out[2] = 13;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9..11].copy_from_slice(&packet_type.to_le_bytes());
    out[11] = page_scan_repetition_mode;
    out[12] = 0; // Reserved (per spec)
    out[13..15].copy_from_slice(&clock_offset.to_le_bytes());
    out[15] = allow_role_switch;
    Ok(out)
}

/// HCI Remote Name Request (Core Spec v5.4 Vol 4 Part E §7.1.19): ask a
/// reachable device for its user-friendly name (e.g. "Lance's Pixel 8"). We
/// issue this right after a phone's ACL comes up so the Device Setup UI can
/// show the phone MODEL instead of a bare MAC — the `RemoteNameRequestComplete`
/// event carries the name back. Page-scan-repetition-mode R1 + unknown clock
/// offset match the create-connection defaults; a device already connected
/// answers from the active link without a fresh page.
pub fn remote_name_request_command(address: &str) -> Result<[u8; 13], String> {
    let mut out = [0u8; 13];
    out[0..2].copy_from_slice(&OPCODE_REMOTE_NAME_REQUEST.to_le_bytes());
    out[2] = 10;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9] = 0x01; // Page Scan Repetition Mode R1
    out[10] = 0x00; // Reserved (was Page Scan Mode)
    out[11..13].copy_from_slice(&0u16.to_le_bytes()); // Clock offset unknown
    Ok(out)
}

pub fn create_connection_command_default(address: &str) -> Result<[u8; 16], String> {
    create_connection_command(
        address, 0xCC18, // BR packet types: DM1 + DH1 + DM3 + DH3 + DM5 + DH5
        0x01,   // Page Scan Repetition Mode R1
        0x0000, // Clock offset unknown
        0x01,   // Allow role switch
    )
}

pub fn accept_connection_request_command(address: &str, role: u8) -> Result<[u8; 10], String> {
    let mut out = [0u8; 10];
    out[0..2].copy_from_slice(&OPCODE_ACCEPT_CONNECTION_REQUEST.to_le_bytes());
    out[2] = 7;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9] = role;
    Ok(out)
}

/// HCI Reject Connection Request (Core Spec v5.4 Vol 4 Part E §7.1.9).
/// Refuse an incoming ACL connection from a device we will not talk to —
/// AOK-BT-001 rejects an UNKNOWN device outside the pairing window. Reason
/// 0x0F = "Connection Rejected due to Unacceptable BD_ADDR", the standard
/// "we don't want this peer" refusal.
pub fn reject_connection_request_command(address: &str) -> Result<[u8; 10], String> {
    let mut out = [0u8; 10];
    out[0..2].copy_from_slice(&OPCODE_REJECT_CONNECTION_REQUEST.to_le_bytes());
    out[2] = 7;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9] = 0x0F; // Connection Rejected due to Unacceptable BD_ADDR
    Ok(out)
}

/// HCI Disconnect (Core Spec v5.4 Vol 4 Part E §7.1.6). Tears down
/// an ACL or SCO connection by handle. Used by the runtime's
/// stage-2 MAS recovery escalation: when force-DISC on dlci 11+4
/// and a fresh Subscribe attempt both fail to wake Pixel, the only
/// remaining lever is to drop the ACL link and let the phone (or
/// the user) re-establish it from scratch.
///
/// Returns synchronously with Command Status; DisconnectionComplete
/// event 0x05 arrives later with the actual reason code from LMP.
///
/// Reason byte: typical values from the spec error code table —
///   0x13 Remote User Terminated Connection (most polite)
///   0x05 Authentication Failure
///   0x16 Connection Terminated By Local Host
/// We use 0x13 so the peer sees us as cleanly hanging up rather
/// than reporting a local fault.
pub fn disconnect_command(connection_handle: u16, reason: u8) -> [u8; 6] {
    let mut out = [0u8; 6];
    out[0..2].copy_from_slice(&OPCODE_DISCONNECT.to_le_bytes());
    out[2] = 3;
    out[3..5].copy_from_slice(&connection_handle.to_le_bytes());
    out[5] = reason;
    out
}

/// HCI Switch_Role (Core Spec v5.4 Vol 4 Part E §7.2.8). Asks the
/// controller to negotiate a role swap on an existing ACL link via
/// LMP. We use this immediately after a successful outbound page
/// (`create_connection_command`) so we, the page initiator (currently
/// master), can become the slave. Without this, Bluedroid PSEs sit
/// passively after L2CAP signalling because their auto-initiate-
/// profiles-on-link-up logic only fires when the AG is the ACL
/// master — observed on Pixel 10a 2026-04-28: page succeeded, ACL up,
/// L2CAP InformationRequest exchanged, then total silence.
///
/// Returns synchronously with Command Status; the actual outcome
/// arrives later as Role Change event 0x12.
///
/// Role byte: 0x00 = become master, 0x01 = become slave.
pub fn switch_role_command(address: &str, role: u8) -> Result<[u8; 10], String> {
    let mut out = [0u8; 10];
    out[0..2].copy_from_slice(&OPCODE_SWITCH_ROLE.to_le_bytes());
    out[2] = 7;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9] = role;
    Ok(out)
}

pub fn accept_synchronous_connection_request_command(
    address: &str,
    transmit_bandwidth: u32,
    receive_bandwidth: u32,
    max_latency: u16,
    voice_setting: u16,
    retransmission_effort: u8,
    packet_types: u16,
) -> Result<[u8; 24], String> {
    let mut out = [0u8; 24];
    out[0..2].copy_from_slice(&OPCODE_ACCEPT_SYNCHRONOUS_CONNECTION_REQUEST.to_le_bytes());
    out[2] = 21;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9..13].copy_from_slice(&transmit_bandwidth.to_le_bytes());
    out[13..17].copy_from_slice(&receive_bandwidth.to_le_bytes());
    out[17..19].copy_from_slice(&max_latency.to_le_bytes());
    out[19..21].copy_from_slice(&voice_setting.to_le_bytes());
    out[21] = retransmission_effort;
    out[22..24].copy_from_slice(&packet_types.to_le_bytes());
    Ok(out)
}

pub fn link_key_request_negative_reply_command(address: &str) -> Result<[u8; 9], String> {
    address_command(OPCODE_LINK_KEY_REQUEST_NEGATIVE_REPLY, address)
}

pub fn pin_code_request_reply_command(address: &str, pin: &str) -> Result<[u8; 26], String> {
    let pin_bytes = pin.as_bytes();
    if pin_bytes.len() > PIN_CODE_PARAM_LEN {
        return Err(format!("PIN code is too long: {} bytes", pin_bytes.len()));
    }

    let mut out = [0u8; 26];
    out[0..2].copy_from_slice(&OPCODE_PIN_CODE_REQUEST_REPLY.to_le_bytes());
    out[2] = 23;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9] = pin_bytes.len() as u8;
    out[10..10 + pin_bytes.len()].copy_from_slice(pin_bytes);
    Ok(out)
}

pub fn pin_code_request_negative_reply_command(address: &str) -> Result<[u8; 9], String> {
    address_command(OPCODE_PIN_CODE_REQUEST_NEGATIVE_REPLY, address)
}

pub fn io_capability_request_reply_command(
    address: &str,
    io_capability: u8,
    oob_data_present: u8,
    authentication_requirements: u8,
) -> Result<[u8; 12], String> {
    let mut out = [0u8; 12];
    out[0..2].copy_from_slice(&OPCODE_IO_CAPABILITY_REQUEST_REPLY.to_le_bytes());
    out[2] = 9;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9] = io_capability;
    out[10] = oob_data_present;
    out[11] = authentication_requirements;
    Ok(out)
}

pub fn io_capability_request_negative_reply_command(
    address: &str,
    reason: u8,
) -> Result<[u8; 10], String> {
    let mut out = [0u8; 10];
    out[0..2].copy_from_slice(&OPCODE_IO_CAPABILITY_REQUEST_NEGATIVE_REPLY.to_le_bytes());
    out[2] = 7;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    out[9] = reason;
    Ok(out)
}

pub fn user_confirmation_request_reply_command(address: &str) -> Result<[u8; 9], String> {
    address_command(OPCODE_USER_CONFIRMATION_REQUEST_REPLY, address)
}

pub fn user_confirmation_request_negative_reply_command(address: &str) -> Result<[u8; 9], String> {
    address_command(OPCODE_USER_CONFIRMATION_REQUEST_NEGATIVE_REPLY, address)
}

pub fn write_default_link_policy_settings_command(settings: u16) -> [u8; 5] {
    let mut out = [0u8; 5];
    out[0..2].copy_from_slice(&OPCODE_WRITE_DEFAULT_LINK_POLICY_SETTINGS.to_le_bytes());
    out[2] = 2;
    out[3..5].copy_from_slice(&settings.to_le_bytes());
    out
}

pub fn write_local_name_command(name: &str) -> [u8; 3 + LOCAL_NAME_PARAM_LEN] {
    let mut out = [0u8; 3 + LOCAL_NAME_PARAM_LEN];
    out[0..2].copy_from_slice(&OPCODE_WRITE_LOCAL_NAME.to_le_bytes());
    out[2] = LOCAL_NAME_PARAM_LEN as u8;
    let bytes = name.as_bytes();
    let len = bytes.len().min(LOCAL_NAME_PARAM_LEN);
    out[3..3 + len].copy_from_slice(&bytes[..len]);
    out
}

pub fn write_page_timeout_command(timeout_slots: u16) -> [u8; 5] {
    let mut out = [0u8; 5];
    out[0..2].copy_from_slice(&OPCODE_WRITE_PAGE_TIMEOUT.to_le_bytes());
    out[2] = 2;
    out[3..5].copy_from_slice(&timeout_slots.to_le_bytes());
    out
}

pub fn write_scan_enable_command(scan_enable: u8) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[0..2].copy_from_slice(&OPCODE_WRITE_SCAN_ENABLE.to_le_bytes());
    out[2] = 1;
    out[3] = scan_enable;
    out
}

pub fn write_class_of_device_command(class_of_device: u32) -> [u8; 6] {
    let mut out = [0u8; 6];
    out[0..2].copy_from_slice(&OPCODE_WRITE_CLASS_OF_DEVICE.to_le_bytes());
    out[2] = 3;
    out[3] = class_of_device as u8;
    out[4] = (class_of_device >> 8) as u8;
    out[5] = (class_of_device >> 16) as u8;
    out
}

pub fn write_voice_setting_command(voice_setting: u16) -> [u8; 5] {
    let mut out = [0u8; 5];
    out[0..2].copy_from_slice(&OPCODE_WRITE_VOICE_SETTING.to_le_bytes());
    out[2] = 2;
    out[3..5].copy_from_slice(&voice_setting.to_le_bytes());
    out
}

/// Broadcom vendor command: configure SCO routing + PCM interface.
///
/// Mirrors BTStack `hci.c:2247` for SCO_OVER_HCI:
/// `hci_send_cmd(&hci_bcm_write_sco_pcm_int, 1, 0, 0, 0, 0)` →
/// Routing=1 (HCI), PCM_Interface_Rate=0 (128k), Frame_Type=0 (short),
/// Sync_Mode=0 (slave), Clock_Mode=0 (slave). Per `hci_cmd.c:2546-2552`.
pub fn write_bcm_sco_pcm_int_command(
    routing: u8,
    pcm_interface_rate: u8,
    frame_type: u8,
    sync_mode: u8,
    clock_mode: u8,
) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0..2].copy_from_slice(&OPCODE_BCM_WRITE_SCO_PCM_INT.to_le_bytes());
    out[2] = 5;
    out[3] = routing;
    out[4] = pcm_interface_rate;
    out[5] = frame_type;
    out[6] = sync_mode;
    out[7] = clock_mode;
    out
}

pub fn write_simple_pairing_mode_command(enabled: bool) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[0..2].copy_from_slice(&OPCODE_WRITE_SIMPLE_PAIRING_MODE.to_le_bytes());
    out[2] = 1;
    out[3] = u8::from(enabled);
    out
}

/// Builds Write Extended Inquiry Response (HCI 7.3.56). The EIR
/// payload tells inquiring peers what services we support before
/// they pair / SDP-query us. Without this, Pixel/Android sees only
/// the Class of Device and never asks our SDP for MAP-MNS or
/// PBAP-PCE — so the per-device "Text messages" / "Contacts and
/// call history" toggles never appear in Settings.
///
/// The HCI parameter block is fixed at 241 bytes: 1 byte FEC flag
/// + 240 bytes EIR data. The EIR data is a sequence of AD-style
/// `{length, type, value...}` triples; unused tail is zero-padded.
pub const EIR_DATA_BYTES: usize = 240;
pub const AD_TYPE_COMPLETE_LIST_16BIT_UUIDS: u8 = 0x03;
pub const AD_TYPE_SHORTENED_LOCAL_NAME: u8 = 0x08;
pub const AD_TYPE_COMPLETE_LOCAL_NAME: u8 = 0x09;

pub fn write_extended_inquiry_response_command(
    fec_required: bool,
    name: &str,
    uuids_16: &[u16],
) -> [u8; 244] {
    let mut out = [0u8; 244];
    out[0..2].copy_from_slice(&OPCODE_WRITE_EXTENDED_INQUIRY_RESPONSE.to_le_bytes());
    out[2] = 1 + EIR_DATA_BYTES as u8;
    out[3] = u8::from(fec_required);

    let eir = &mut out[4..];
    let mut cursor = 0;

    if !uuids_16.is_empty() {
        let value_len = uuids_16.len() * 2;
        if cursor + 2 + value_len <= eir.len() {
            eir[cursor] = (1 + value_len) as u8;
            eir[cursor + 1] = AD_TYPE_COMPLETE_LIST_16BIT_UUIDS;
            cursor += 2;
            for uuid in uuids_16 {
                eir[cursor..cursor + 2].copy_from_slice(&uuid.to_le_bytes());
                cursor += 2;
            }
        }
    }

    let name_bytes = name.as_bytes();
    let name_room = eir.len().saturating_sub(cursor + 2);
    let name_len = name_bytes.len().min(name_room);
    if name_len > 0 {
        let ad_type = if name_len < name_bytes.len() {
            AD_TYPE_SHORTENED_LOCAL_NAME
        } else {
            AD_TYPE_COMPLETE_LOCAL_NAME
        };
        eir[cursor] = (1 + name_len) as u8;
        eir[cursor + 1] = ad_type;
        cursor += 2;
        eir[cursor..cursor + name_len].copy_from_slice(&name_bytes[..name_len]);
    }

    out
}

fn command_no_params(opcode: u16) -> [u8; 3] {
    let [lo, hi] = opcode.to_le_bytes();
    [lo, hi, 0]
}

fn address_command(opcode: u16, address: &str) -> Result<[u8; 9], String> {
    let mut out = [0u8; 9];
    out[0..2].copy_from_slice(&opcode.to_le_bytes());
    out[2] = 6;
    out[3..9].copy_from_slice(&parse_bd_addr(address)?);
    Ok(out)
}

pub fn parse_event(packet: &[u8]) -> Result<HciPacket<'_>, String> {
    if packet.len() < 2 {
        return Err("HCI event packet too short".to_string());
    }
    let event_code = packet[0];
    let len = packet[1] as usize;
    if packet.len() < 2 + len {
        return Err(format!(
            "HCI event declares {} bytes but packet has {} payload bytes",
            len,
            packet.len().saturating_sub(2)
        ));
    }
    Ok(HciPacket::Event {
        event_code,
        params: &packet[2..2 + len],
    })
}

pub fn parse_acl(packet: &[u8]) -> Result<HciPacket<'_>, String> {
    if packet.len() < 4 {
        return Err("HCI ACL packet too short".to_string());
    }
    let handle_pb_bc = u16::from_le_bytes([packet[0], packet[1]]);
    let len = u16::from_le_bytes([packet[2], packet[3]]) as usize;
    if packet.len() < 4 + len {
        return Err(format!(
            "HCI ACL packet declares {} bytes but packet has {} payload bytes",
            len,
            packet.len().saturating_sub(4)
        ));
    }
    Ok(HciPacket::AclData {
        handle_pb_bc,
        payload: &packet[4..4 + len],
    })
}

pub fn parse_sco(packet: &[u8]) -> Result<HciPacket<'_>, String> {
    if packet.len() < 3 {
        return Err("HCI SCO packet too short".to_string());
    }
    let handle_status = u16::from_le_bytes([packet[0], packet[1]]);
    let len = packet[2] as usize;
    if packet.len() < 3 + len {
        return Err(format!(
            "HCI SCO packet declares {} bytes but packet has {} payload bytes",
            len,
            packet.len().saturating_sub(3)
        ));
    }
    Ok(HciPacket::ScoData {
        handle_status,
        payload: &packet[3..3 + len],
    })
}

pub fn parse_command_complete(packet: &[u8], expected_opcode: u16) -> Result<&[u8], String> {
    let HciPacket::Event { event_code, params } = parse_event(packet)? else {
        unreachable!();
    };
    if event_code != 0x0e {
        return Err(format!(
            "expected Command Complete event 0x0e, got 0x{:02x}",
            event_code
        ));
    }
    if params.len() < 3 {
        return Err("Command Complete event is too short".to_string());
    }
    let opcode = u16::from_le_bytes([params[1], params[2]]);
    if opcode != expected_opcode {
        return Err(format!(
            "expected Command Complete for opcode 0x{:04x}, got 0x{:04x}",
            expected_opcode, opcode
        ));
    }
    Ok(&params[3..])
}

pub fn parse_typed_event(packet: &[u8]) -> Result<HciEvent, String> {
    let HciPacket::Event { event_code, params } = parse_event(packet)? else {
        unreachable!();
    };
    parse_event_params(event_code, params)
}

pub fn parse_event_params(event_code: u8, params: &[u8]) -> Result<HciEvent, String> {
    match event_code {
        EVENT_COMMAND_COMPLETE => {
            require_len(params, 3, "Command Complete")?;
            Ok(HciEvent::CommandComplete {
                num_hci_command_packets: params[0],
                opcode: u16::from_le_bytes([params[1], params[2]]),
                return_params: params[3..].to_vec(),
            })
        }
        EVENT_COMMAND_STATUS => {
            require_len(params, 4, "Command Status")?;
            Ok(HciEvent::CommandStatus {
                status: params[0],
                num_hci_command_packets: params[1],
                opcode: u16::from_le_bytes([params[2], params[3]]),
            })
        }
        EVENT_CONNECTION_COMPLETE => {
            require_len(params, 11, "Connection Complete")?;
            Ok(HciEvent::ConnectionComplete {
                status: params[0],
                connection_handle: u16::from_le_bytes([params[1], params[2]]),
                address: format_bd_addr(&params[3..9]),
                link_type: params[9],
                encryption_enabled: params[10],
            })
        }
        EVENT_CONNECTION_REQUEST => {
            require_len(params, 10, "Connection Request")?;
            Ok(HciEvent::ConnectionRequest {
                address: format_bd_addr(&params[0..6]),
                class_of_device: (params[6] as u32)
                    | ((params[7] as u32) << 8)
                    | ((params[8] as u32) << 16),
                link_type: params[9],
            })
        }
        EVENT_DISCONNECTION_COMPLETE => {
            require_len(params, 4, "Disconnection Complete")?;
            Ok(HciEvent::DisconnectionComplete {
                status: params[0],
                connection_handle: u16::from_le_bytes([params[1], params[2]]),
                reason: params[3],
            })
        }
        EVENT_REMOTE_NAME_REQUEST_COMPLETE => {
            require_len(params, 7, "Remote Name Request Complete")?;
            Ok(HciEvent::RemoteNameRequestComplete {
                status: params[0],
                address: format_bd_addr(&params[1..7]),
                name: parse_null_terminated_name(&params[7..]),
            })
        }
        EVENT_PIN_CODE_REQUEST => {
            require_len(params, 6, "PIN Code Request")?;
            Ok(HciEvent::PinCodeRequest {
                address: format_bd_addr(&params[0..6]),
            })
        }
        EVENT_LINK_KEY_REQUEST => {
            require_len(params, 6, "Link Key Request")?;
            Ok(HciEvent::LinkKeyRequest {
                address: format_bd_addr(&params[0..6]),
            })
        }
        EVENT_LINK_KEY_NOTIFICATION => {
            require_len(params, 23, "Link Key Notification")?;
            let mut link_key = [0u8; 16];
            link_key.copy_from_slice(&params[6..22]);
            Ok(HciEvent::LinkKeyNotification {
                address: format_bd_addr(&params[0..6]),
                link_key,
                key_type: params[22],
            })
        }
        EVENT_SYNCHRONOUS_CONNECTION_COMPLETE => {
            require_len(params, 17, "Synchronous Connection Complete")?;
            Ok(HciEvent::SynchronousConnectionComplete {
                status: params[0],
                connection_handle: u16::from_le_bytes([params[1], params[2]]),
                address: format_bd_addr(&params[3..9]),
                link_type: params[9],
                transmission_interval: params[10],
                retransmission_window: params[11],
                rx_packet_length: u16::from_le_bytes([params[12], params[13]]),
                tx_packet_length: u16::from_le_bytes([params[14], params[15]]),
                air_mode: params[16],
            })
        }
        EVENT_IO_CAPABILITY_REQUEST => {
            require_len(params, 6, "IO Capability Request")?;
            Ok(HciEvent::IoCapabilityRequest {
                address: format_bd_addr(&params[0..6]),
            })
        }
        EVENT_USER_CONFIRMATION_REQUEST => {
            require_len(params, 10, "User Confirmation Request")?;
            Ok(HciEvent::UserConfirmationRequest {
                address: format_bd_addr(&params[0..6]),
                numeric_value: u32::from_le_bytes([params[6], params[7], params[8], params[9]]),
            })
        }
        EVENT_SIMPLE_PAIRING_COMPLETE => {
            require_len(params, 7, "Simple Pairing Complete")?;
            Ok(HciEvent::SimplePairingComplete {
                status: params[0],
                address: format_bd_addr(&params[1..7]),
            })
        }
        _ => Ok(HciEvent::Unknown {
            event_code,
            params: params.to_vec(),
        }),
    }
}

fn require_len(params: &[u8], min_len: usize, event_name: &str) -> Result<(), String> {
    if params.len() < min_len {
        return Err(format!(
            "{} event is too short: expected at least {} bytes, got {}",
            event_name,
            min_len,
            params.len()
        ));
    }
    Ok(())
}

fn format_bd_addr(bytes: &[u8]) -> String {
    debug_assert!(bytes.len() >= 6);
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        bytes[5], bytes[4], bytes[3], bytes[2], bytes[1], bytes[0]
    )
}

fn parse_null_terminated_name(bytes: &[u8]) -> String {
    let len = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

pub fn parse_read_bd_addr_return(params: &[u8]) -> Result<String, String> {
    if params.len() < 7 {
        return Err("Read BD_ADDR return parameters are too short".to_string());
    }
    let status = params[0];
    if status != 0 {
        return Err(format!("Read BD_ADDR failed with status 0x{:02x}", status));
    }
    Ok(format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        params[6], params[5], params[4], params[3], params[2], params[1]
    ))
}

pub fn expect_status_ok(params: &[u8], operation: &str) -> Result<(), String> {
    let status = params
        .first()
        .copied()
        .ok_or_else(|| format!("{} returned no status byte", operation))?;
    if status != 0 {
        return Err(format!("{} failed with status 0x{:02x}", operation, status));
    }
    Ok(())
}

pub fn parse_local_version_return(params: &[u8]) -> Result<LocalVersion, String> {
    if params.len() < 9 {
        return Err("Read Local Version return parameters are too short".to_string());
    }
    expect_status_ok(params, "Read Local Version")?;
    Ok(LocalVersion {
        hci_version: params[1],
        hci_revision: u16::from_le_bytes([params[2], params[3]]),
        lmp_pal_version: params[4],
        manufacturer_name: u16::from_le_bytes([params[5], params[6]]),
        lmp_pal_subversion: u16::from_le_bytes([params[7], params[8]]),
    })
}

pub fn parse_local_supported_features_return(params: &[u8]) -> Result<[u8; 8], String> {
    if params.len() < 9 {
        return Err("Read Local Supported Features return parameters are too short".to_string());
    }
    expect_status_ok(params, "Read Local Supported Features")?;
    let mut features = [0u8; 8];
    features.copy_from_slice(&params[1..9]);
    Ok(features)
}

pub fn parse_buffer_size_return(params: &[u8]) -> Result<BufferSize, String> {
    if params.len() < 8 {
        return Err("Read Buffer Size return parameters are too short".to_string());
    }
    expect_status_ok(params, "Read Buffer Size")?;
    Ok(BufferSize {
        acl_data_packet_length: u16::from_le_bytes([params[1], params[2]]),
        sco_data_packet_length: params[3],
        total_num_acl_data_packets: u16::from_le_bytes([params[4], params[5]]),
        total_num_sco_data_packets: u16::from_le_bytes([params[6], params[7]]),
    })
}

pub fn parse_bd_addr(address: &str) -> Result<[u8; 6], String> {
    let mut out = [0u8; 6];
    let mut count = 0usize;
    for (index, part) in address.split(':').enumerate() {
        if index >= 6 {
            return Err(format!("Bluetooth address has too many parts: {}", address));
        }
        if part.len() != 2 {
            return Err(format!(
                "Bluetooth address part '{}' is not two hex digits",
                part
            ));
        }
        out[5 - index] = u8::from_str_radix(part, 16)
            .map_err(|_| format!("Bluetooth address part '{}' is not valid hex", part))?;
        count += 1;
    }
    if count != 6 {
        return Err(format!("Bluetooth address has {} parts, expected 6", count));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builders_match_hci_opcodes() {
        assert_eq!(reset_command(), [0x03, 0x0c, 0x00]);
        assert_eq!(read_bd_addr_command(), [0x09, 0x10, 0x00]);
        assert_eq!(read_local_version_information_command(), [0x01, 0x10, 0x00]);
        assert_eq!(read_local_supported_features_command(), [0x03, 0x10, 0x00]);
        assert_eq!(read_buffer_size_command(), [0x05, 0x10, 0x00]);
        assert_eq!(
            write_default_link_policy_settings_command(0x0005),
            [0x0f, 0x08, 0x02, 0x05, 0x00]
        );
        assert_eq!(
            write_page_timeout_command(0x6000),
            [0x18, 0x0c, 0x02, 0x00, 0x60]
        );
        assert_eq!(write_scan_enable_command(0x03), [0x1a, 0x0c, 0x01, 0x03]);
        assert_eq!(
            write_class_of_device_command(0x200408),
            [0x24, 0x0c, 0x03, 0x08, 0x04, 0x20]
        );
        assert_eq!(
            write_voice_setting_command(0x0060),
            [0x26, 0x0c, 0x02, 0x60, 0x00]
        );
        assert_eq!(
            write_simple_pairing_mode_command(true),
            [0x56, 0x0c, 0x01, 0x01]
        );
        assert_eq!(
            link_key_request_negative_reply_command("00:19:86:00:22:6C").unwrap(),
            [0x0c, 0x04, 0x06, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00]
        );
        assert_eq!(
            pin_code_request_negative_reply_command("00:19:86:00:22:6C").unwrap(),
            [0x0e, 0x04, 0x06, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00]
        );
        assert_eq!(
            user_confirmation_request_reply_command("00:19:86:00:22:6C").unwrap(),
            [0x2c, 0x04, 0x06, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00]
        );
        assert_eq!(
            set_event_mask_command(0x3fffffff_ffffffff),
            [0x01, 0x0c, 0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x3f]
        );
    }

    #[test]
    fn pairing_command_builders_match_hci_layouts() {
        let mut link_key = [0u8; 16];
        for (index, byte) in link_key.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let command = link_key_request_reply_command("00:19:86:00:22:6C", &link_key).unwrap();
        assert_eq!(
            &command[0..9],
            &[0x0b, 0x04, 0x16, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00]
        );
        assert_eq!(&command[9..25], &link_key);

        let command = pin_code_request_reply_command("00:19:86:00:22:6C", "0000").unwrap();
        assert_eq!(
            &command[0..10],
            &[0x0d, 0x04, 0x17, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 4]
        );
        assert_eq!(&command[10..14], b"0000");
        assert!(command[14..].iter().all(|b| *b == 0));

        assert_eq!(
            io_capability_request_reply_command(
                "00:19:86:00:22:6C",
                SSP_IO_CAPABILITY_NO_INPUT_NO_OUTPUT,
                SSP_OOB_DATA_NOT_PRESENT,
                SSP_AUTHREQ_MITM_NOT_REQUIRED_GENERAL_BONDING,
            )
            .unwrap(),
            [0x2b, 0x04, 0x09, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 0x03, 0x00, 0x04]
        );
        assert_eq!(
            io_capability_request_negative_reply_command("00:19:86:00:22:6C", 0x18).unwrap(),
            [0x34, 0x04, 0x07, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 0x18]
        );
    }

    #[test]
    fn connection_accept_command_builders_match_hci_layouts() {
        assert_eq!(
            accept_connection_request_command("00:19:86:00:22:6C", ACCEPT_ROLE_REMAIN_SLAVE)
                .unwrap(),
            [0x09, 0x04, 0x07, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 0x01]
        );

        assert_eq!(
            accept_synchronous_connection_request_command(
                "00:19:86:00:22:6C",
                8000,
                8000,
                0xffff,
                0x0060,
                0xff,
                SCO_PACKET_TYPES_HFP_CVSD_ESCO_COMMAND,
            )
            .unwrap(),
            [
                0x29, 0x04, 0x15, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 0x40, 0x1f, 0x00, 0x00, 0x40,
                0x1f, 0x00, 0x00, 0xff, 0xff, 0x60, 0x00, 0xff, 0x88, 0x03,
            ]
        );
    }

    #[test]
    fn parses_bluetooth_address_strings() {
        assert_eq!(
            parse_bd_addr("00:19:86:00:22:6C").unwrap(),
            [0x6c, 0x22, 0x00, 0x86, 0x19, 0x00]
        );
        assert!(parse_bd_addr("00:19:86:00:22").is_err());
        assert!(parse_bd_addr("00:19:86:00:22:ZZ").is_err());
    }

    #[test]
    fn write_local_name_command_pads_to_hci_name_length() {
        let command = write_local_name_command("Aokie AI Assistant");
        assert_eq!(command[0..3], [0x13, 0x0c, 248]);
        assert_eq!(&command[3..21], b"Aokie AI Assistant");
        assert!(command[21..].iter().all(|b| *b == 0));

        let long_name = "x".repeat(300);
        let command = write_local_name_command(&long_name);
        assert_eq!(command.len(), 251);
        assert!(command[3..].iter().all(|b| *b == b'x'));
    }

    #[test]
    fn write_eir_packs_uuids_then_name_and_zero_pads() {
        let command =
            write_extended_inquiry_response_command(false, "Aokie", &[0x111e, 0x1133, 0x1200]);
        assert_eq!(command[0..3], [0x52, 0x0c, 241]);
        assert_eq!(command[3], 0x00);
        // UUID list AD: length=7, type=0x03, then three little-endian uuid16s.
        assert_eq!(command[4], 7);
        assert_eq!(command[5], AD_TYPE_COMPLETE_LIST_16BIT_UUIDS);
        assert_eq!(&command[6..12], &[0x1e, 0x11, 0x33, 0x11, 0x00, 0x12]);
        // Complete Local Name AD: length=6, type=0x09, "Aokie".
        assert_eq!(command[12], 6);
        assert_eq!(command[13], AD_TYPE_COMPLETE_LOCAL_NAME);
        assert_eq!(&command[14..19], b"Aokie");
        // Tail is zero-padded out to the full 240-byte EIR window.
        assert!(command[19..].iter().all(|b| *b == 0));
    }

    #[test]
    fn write_eir_truncates_long_name_with_shortened_ad_type() {
        let long_name = "x".repeat(300);
        let command = write_extended_inquiry_response_command(true, &long_name, &[]);
        assert_eq!(command[3], 0x01);
        // No UUID AD struct, so name AD starts at the very first EIR byte.
        let name_room = 240 - 2;
        assert_eq!(command[4] as usize, 1 + name_room);
        assert_eq!(command[5], AD_TYPE_SHORTENED_LOCAL_NAME);
        assert!(command[6..6 + name_room].iter().all(|b| *b == b'x'));
    }

    #[test]
    fn parses_event_packets() {
        let packet = parse_event(&[0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00]).unwrap();
        assert_eq!(
            packet,
            HciPacket::Event {
                event_code: 0x0e,
                params: &[0x01, 0x03, 0x0c, 0x00],
            }
        );
    }

    #[test]
    fn rejects_truncated_acl_packets() {
        let err = parse_acl(&[0x01, 0x20, 0x05, 0x00, 0xaa]).unwrap_err();
        assert!(err.contains("declares 5 bytes"));
    }

    #[test]
    fn parses_packet_type_byte() {
        assert_eq!(PacketType::from_u8(0x04), Some(PacketType::Event));
        assert_eq!(PacketType::from_u8(0xff), None);
    }

    #[test]
    fn parses_command_complete_return_params() {
        let params = parse_command_complete(&[0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00], 0x0c03).unwrap();
        assert_eq!(params, &[0x00]);
    }

    #[test]
    fn parses_typed_command_events() {
        assert_eq!(
            parse_typed_event(&[0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00]).unwrap(),
            HciEvent::CommandComplete {
                num_hci_command_packets: 1,
                opcode: 0x0c03,
                return_params: vec![0],
            }
        );
        assert_eq!(
            parse_typed_event(&[0x0f, 0x04, 0x00, 0x01, 0x05, 0x04]).unwrap(),
            HciEvent::CommandStatus {
                status: 0,
                num_hci_command_packets: 1,
                opcode: 0x0405,
            }
        );
    }

    #[test]
    fn parses_typed_classic_connection_events() {
        let connection = parse_typed_event(&[
            0x03, 0x0b, 0x00, 0x2a, 0x00, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 0x01, 0x00,
        ])
        .unwrap();
        assert_eq!(
            connection,
            HciEvent::ConnectionComplete {
                status: 0,
                connection_handle: 0x002a,
                address: "00:19:86:00:22:6C".to_string(),
                link_type: 1,
                encryption_enabled: 0,
            }
        );

        let request = parse_typed_event(&[
            0x04, 0x0a, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 0x08, 0x04, 0x20, 0x01,
        ])
        .unwrap();
        assert_eq!(
            request,
            HciEvent::ConnectionRequest {
                address: "00:19:86:00:22:6C".to_string(),
                class_of_device: 0x200408,
                link_type: 1,
            }
        );

        let disconnect = parse_typed_event(&[0x05, 0x04, 0x00, 0x2a, 0x00, 0x13]).unwrap();
        assert_eq!(
            disconnect,
            HciEvent::DisconnectionComplete {
                status: 0,
                connection_handle: 0x002a,
                reason: 0x13,
            }
        );
    }

    #[test]
    fn parses_typed_pairing_events() {
        assert_eq!(
            parse_typed_event(&[0x31, 0x06, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00]).unwrap(),
            HciEvent::IoCapabilityRequest {
                address: "00:19:86:00:22:6C".to_string(),
            }
        );
        assert_eq!(
            parse_typed_event(&[
                0x33, 0x0a, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 0x40, 0xe2, 0x01, 0x00
            ])
            .unwrap(),
            HciEvent::UserConfirmationRequest {
                address: "00:19:86:00:22:6C".to_string(),
                numeric_value: 123456,
            }
        );

        let mut link_key_event = vec![0x18, 0x17, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00];
        link_key_event.extend(0u8..16);
        link_key_event.push(0x04);
        assert_eq!(
            parse_typed_event(&link_key_event).unwrap(),
            HciEvent::LinkKeyNotification {
                address: "00:19:86:00:22:6C".to_string(),
                link_key: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
                key_type: 0x04,
            }
        );
    }

    #[test]
    fn parses_typed_sco_event_and_unknown_event() {
        let sco = parse_typed_event(&[
            0x2c, 0x11, 0x00, 0x40, 0x00, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00, 0x02, 0x0c, 0x06,
            0x3c, 0x00, 0x3c, 0x00, 0x02,
        ])
        .unwrap();
        assert_eq!(
            sco,
            HciEvent::SynchronousConnectionComplete {
                status: 0,
                connection_handle: 0x0040,
                address: "00:19:86:00:22:6C".to_string(),
                link_type: 2,
                transmission_interval: 0x0c,
                retransmission_window: 0x06,
                rx_packet_length: 60,
                tx_packet_length: 60,
                air_mode: 2,
            }
        );

        assert_eq!(
            parse_typed_event(&[0xff, 0x02, 0x01, 0x02]).unwrap(),
            HciEvent::Unknown {
                event_code: 0xff,
                params: vec![1, 2],
            }
        );
    }

    #[test]
    fn parses_read_bd_addr_response() {
        let address =
            parse_read_bd_addr_return(&[0x00, 0x6c, 0x22, 0x00, 0x86, 0x19, 0x00]).unwrap();
        assert_eq!(address, "00:19:86:00:22:6C");
    }

    #[test]
    fn create_connection_command_packs_address_and_defaults_in_le_order() {
        let bytes = create_connection_command_default("04:C8:B0:CB:B9:D1").unwrap();
        // Opcode 0x0405 little-endian, then param length 13.
        assert_eq!(&bytes[0..3], &[0x05, 0x04, 0x0d]);
        // BD_ADDR is reversed for the wire: lowest byte first.
        assert_eq!(&bytes[3..9], &[0xd1, 0xb9, 0xcb, 0xb0, 0xc8, 0x04]);
        // Packet type 0xCC18 little-endian.
        assert_eq!(&bytes[9..11], &[0x18, 0xcc]);
        // Page Scan Repetition Mode R1.
        assert_eq!(bytes[11], 0x01);
        // Reserved (must be zero per spec).
        assert_eq!(bytes[12], 0x00);
        // Clock offset zero (unknown).
        assert_eq!(&bytes[13..15], &[0x00, 0x00]);
        // Allow role switch.
        assert_eq!(bytes[15], 0x01);
    }

    #[test]
    fn remote_name_request_command_packs_address_and_defaults() {
        let bytes = remote_name_request_command("04:C8:B0:E1:3F:F3").unwrap();
        // Opcode 0x0419 little-endian, then param length 10.
        assert_eq!(&bytes[0..3], &[0x19, 0x04, 0x0a]);
        // BD_ADDR little-endian.
        assert_eq!(&bytes[3..9], &[0xf3, 0x3f, 0xe1, 0xb0, 0xc8, 0x04]);
        assert_eq!(bytes[9], 0x01); // PSRM R1
        assert_eq!(bytes[10], 0x00); // reserved
        assert_eq!(&bytes[11..13], &[0x00, 0x00]); // clock offset unknown
    }

    #[test]
    fn disconnect_command_packs_handle_and_reason() {
        let bytes = disconnect_command(0x000b, 0x13);
        // Opcode 0x0406 little-endian, then param length 3.
        assert_eq!(&bytes[0..3], &[0x06, 0x04, 0x03]);
        // Connection handle little-endian.
        assert_eq!(&bytes[3..5], &[0x0b, 0x00]);
        // Reason byte: 0x13 Remote User Terminated Connection.
        assert_eq!(bytes[5], 0x13);
    }

    #[test]
    fn switch_role_command_packs_address_and_role_byte() {
        let bytes = switch_role_command("04:C8:B0:CB:B9:D1", 0x01).unwrap();
        // Opcode 0x080b little-endian, then param length 7.
        assert_eq!(&bytes[0..3], &[0x0b, 0x08, 0x07]);
        // BD_ADDR reversed for the wire.
        assert_eq!(&bytes[3..9], &[0xd1, 0xb9, 0xcb, 0xb0, 0xc8, 0x04]);
        // Role byte: 0x01 = become slave.
        assert_eq!(bytes[9], 0x01);
    }

    #[test]
    fn parses_controller_info_responses() {
        let version =
            parse_local_version_return(&[0x00, 0x0b, 0x34, 0x12, 0x0b, 0x5d, 0x00, 0x78, 0x56])
                .unwrap();
        assert_eq!(version.hci_version, 0x0b);
        assert_eq!(version.hci_revision, 0x1234);
        assert_eq!(version.manufacturer_name, 0x005d);

        let features =
            parse_local_supported_features_return(&[0x00, 1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(features, [1, 2, 3, 4, 5, 6, 7, 8]);

        let buffers =
            parse_buffer_size_return(&[0x00, 0xff, 0x03, 0x3c, 0x08, 0x00, 0x04, 0x00]).unwrap();
        assert_eq!(buffers.acl_data_packet_length, 1023);
        assert_eq!(buffers.sco_data_packet_length, 0x3c);
        assert_eq!(buffers.total_num_acl_data_packets, 8);
        assert_eq!(buffers.total_num_sco_data_packets, 4);
    }

    #[test]
    fn fuzz_event_acl_sco_parsers_do_not_panic_on_random_bytes() {
        // The HCI packet parsers are fed straight from WinUSB reads;
        // a malformed event from the controller (or a misaligned
        // accumulator) must surface as Err, never a panic.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x4843_4900_4843_4900);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 512) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_event(&buf);
            let _ = parse_acl(&buf);
            let _ = parse_sco(&buf);
            let _ = parse_typed_event(&buf);
        }
    }

    #[test]
    fn fuzz_event_params_does_not_panic_across_event_codes() {
        // parse_event_params dispatches on the event code (u8); make
        // sure every code × random-payload combo stays panic-free.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xdead_beef_cafe_babe);
        for _ in 0..5_000 {
            let event_code = (rng.next_u32() & 0xff) as u8;
            let len = (rng.next_u32() % 256) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_event_params(event_code, &buf);
        }
    }

    #[test]
    fn fuzz_command_complete_does_not_panic_on_random_bytes() {
        // parse_command_complete also looks up an opcode — random
        // bytes must not crash regardless of expected_opcode.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xc0de_b00c_b00c_c0de);
        for _ in 0..2_000 {
            let len = (rng.next_u32() % 128) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let opcode = (rng.next_u32() & 0xffff) as u16;
            let _ = parse_command_complete(&buf, opcode);
        }
    }

    #[test]
    fn fuzz_bd_addr_parser_does_not_panic_on_random_strings() {
        // parse_bd_addr (string → 6 bytes) is also user-facing in
        // a few command paths; arbitrary strings must Err, not panic.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xadde_baad_adde_baad);
        for _ in 0..1_000 {
            let len = (rng.next_u32() % 32) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let s: String = buf
                .iter()
                .map(|b| char::from(b.saturating_add(0x20).min(0x7e)))
                .collect();
            let _ = parse_bd_addr(&s);
        }
    }
}
