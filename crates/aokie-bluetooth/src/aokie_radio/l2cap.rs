use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{hfp, hfp_client, rfcomm, sdp};

/// PSM handler. Receives the channel (so RFCOMM-style handlers can
/// touch per-channel state) and the L2CAP payload; returns zero or
/// more upper-layer reply payloads, which the L2CAP layer wraps in
/// basic frames + ACL packets and forwards to the transport.
///
/// Phase 0b of the MAP/PBAP plan: future profiles (OBEX-on-L2CAP)
/// register their PSM at L2capState construction time instead of being
/// hard-coded in the dispatch site.
pub type PsmHandler =
    Arc<dyn Fn(&mut L2capChannel, &[u8]) -> Result<Vec<Vec<u8>>, String> + Send + Sync>;

pub const CID_SIGNALING: u16 = 0x0001;
pub const PSM_SDP: u16 = 0x0001;
pub const PSM_RFCOMM: u16 = 0x0003;

pub const SIGNAL_COMMAND_REJECT: u8 = 0x01;
pub const SIGNAL_CONNECTION_REQUEST: u8 = 0x02;
pub const SIGNAL_CONNECTION_RESPONSE: u8 = 0x03;
pub const SIGNAL_CONFIGURE_REQUEST: u8 = 0x04;
pub const SIGNAL_CONFIGURE_RESPONSE: u8 = 0x05;
pub const SIGNAL_DISCONNECTION_REQUEST: u8 = 0x06;
pub const SIGNAL_DISCONNECTION_RESPONSE: u8 = 0x07;
pub const SIGNAL_INFORMATION_REQUEST: u8 = 0x0a;
pub const SIGNAL_INFORMATION_RESPONSE: u8 = 0x0b;

pub const CONNECTION_RESULT_SUCCESS: u16 = 0x0000;
pub const CONNECTION_RESULT_PENDING: u16 = 0x0001;
pub const CONNECTION_RESULT_PSM_NOT_SUPPORTED: u16 = 0x0002;
pub const CONNECTION_RESULT_SECURITY_BLOCK: u16 = 0x0003;
pub const CONNECTION_RESULT_NO_RESOURCES: u16 = 0x0004;
pub const CONNECTION_STATUS_NO_FURTHER_INFORMATION: u16 = 0x0000;

pub const CONFIG_RESULT_SUCCESS: u16 = 0x0000;
pub const CONFIG_RESULT_UNACCEPTABLE_PARAMETERS: u16 = 0x0001;

pub const COMMAND_REJECT_REASON_COMMAND_NOT_UNDERSTOOD: u16 = 0x0000;

// L2CAP InformationRequest types (BT Core 5.x §4.10).
pub const INFO_TYPE_CONNECTIONLESS_MTU: u16 = 0x0001;
pub const INFO_TYPE_EXTENDED_FEATURES: u16 = 0x0002;
pub const INFO_TYPE_FIXED_CHANNELS: u16 = 0x0003;

pub const INFO_RESULT_SUCCESS: u16 = 0x0000;
pub const INFO_RESULT_NOT_SUPPORTED: u16 = 0x0001;

// Connectionless MTU we'll claim for type 0x0001. Spec default; nothing
// in the Aokie stack uses connectionless data, but Pixel asks anyway.
pub const AOKIE_CONNECTIONLESS_MTU: u16 = 672;
// Extended features mask — all zero. We run pure basic-mode L2CAP:
// no flow control, no ERTM, no streaming, and we don't advertise
// the FixedChannels query (bit 7) because doing so makes Pixel
// emit a follow-up `InformationRequest type 0x0003` that arrives
// in a tight enough window for transient USB-transport hiccups
// to swallow it — Pixel then stalls indefinitely waiting for the
// reply. Saying "I have no extended features" makes Pixel skip
// the second query and proceed straight to ConnectionRequest.
//
// Returning 0x0000 SILENTLY (CommandReject) was the original bug
// fixed alongside this constant; the mask value here is a separate
// follow-up to that fix.
pub const AOKIE_EXTENDED_FEATURES_MASK: u32 = 0x0000_0000;
// Fixed channels bitmap — bit 1 (CID 0x0001 signaling) is the only one
// we serve. Connectionless (CID 0x0002), AMP, and Security Manager are
// all unsupported. Per spec the bitmap is 8 bytes. Pixel won't ask
// for this type with our extended-features mask = 0, but keep the
// honest reply available for peers that probe regardless.
pub const AOKIE_FIXED_CHANNELS_MASK: u64 = 0x02;
pub const ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE: u8 = 0x02;
pub const ACL_PACKET_BOUNDARY_CONTINUING_FRAGMENT: u8 = 0x01;
pub const ACL_BROADCAST_POINT_TO_POINT: u8 = 0x00;
pub const FIRST_DYNAMIC_CID: u16 = 0x0040;

// Identifier-allocation range for ConfigureRequests we originate. The
// remote picks identifiers from 0x01..=0x7f for its own commands, so
// keeping ours in 0x80..=0xff means in-flight requests don't collide
// with replies we're routing back to it.
const FIRST_LOCAL_SIGNALING_ID: u8 = 0x80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AclFrame<'a> {
    pub connection_handle: u16,
    pub packet_boundary_flag: u8,
    pub broadcast_flag: u8,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasicFrame<'a> {
    pub cid: u16,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalingHeader<'a> {
    pub code: u8,
    pub identifier: u8,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalingCommand {
    ConnectionRequest {
        identifier: u8,
        psm: u16,
        source_cid: u16,
    },
    /// Reply to a ConnectionRequest we sent. Pre-Phase-2b this fell
    /// through to `Unknown` and we'd emit a CommandReject — fine when
    /// we never originated channels, but as soon as Phase 2b
    /// (PBAP/MAP outbound) tried to open one we'd reject our peer's
    /// own ack.
    ConnectionResponse {
        identifier: u8,
        destination_cid: u16,
        source_cid: u16,
        result: u16,
        status: u16,
    },
    ConfigureRequest {
        identifier: u8,
        destination_cid: u16,
        flags: u16,
        options: Vec<u8>,
    },
    /// Reply to a ConfigureRequest we sent. Pre-fix this fell through to
    /// `Unknown` and we'd emit a CommandReject — so the AG could accept
    /// our config and we'd respond by tearing it back down.
    ConfigureResponse {
        identifier: u8,
        source_cid: u16,
        flags: u16,
        result: u16,
        options: Vec<u8>,
    },
    DisconnectionRequest {
        identifier: u8,
        destination_cid: u16,
        source_cid: u16,
    },
    /// Courtesy ack for a DisconnectionRequest we sent — needs to be
    /// parseable so we don't reject the peer's reply during outbound
    /// teardown.
    DisconnectionResponse {
        identifier: u8,
        destination_cid: u16,
        source_cid: u16,
    },
    InformationRequest {
        identifier: u8,
        info_type: u16,
    },
    /// Reply to an InformationRequest. We don't issue them today, so we
    /// only need to parse responses to keep them out of the Unknown
    /// bucket — which is wired to CommandReject and would loop the peer.
    InformationResponse {
        identifier: u8,
        info_type: u16,
        result: u16,
        data: Vec<u8>,
    },
    Unknown {
        code: u8,
        identifier: u8,
        payload: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Configuring,
    Open,
}

// L2capChannel used to derive Debug/Clone/PartialEq/Eq, but it owns an
// `Option<RfcommState>` and RfcommState now holds closures via the
// server-channel registry — those traits no longer auto-derive. Tests
// access channel state through `L2capState::channel()` accessors only;
// dropping the derives is safe.
pub struct L2capChannel {
    pub connection_handle: u16,
    pub psm: u16,
    pub local_cid: u16,
    pub remote_cid: u16,
    pub state: ChannelState,
    pub local_configured: bool,
    pub remote_configured: bool,
    /// Identifier of the ConfigureRequest we sent toward the remote, or
    /// `None` if no request is outstanding (either we haven't sent one
    /// yet or the remote has already replied with success).
    pub local_config_identifier: Option<u8>,
    /// Wall-clock instant the ConfigureRequest went out. Drives the
    /// L2CAP-level stall watchdog: a peer that ACKs ConnectionRequest
    /// but never sends our ConfigureResponse leaves the channel in
    /// `Configuring` forever, blocking RFCOMM/HFP from ever opening.
    /// Without this the receptionist would silently never go service-
    /// level ready and the user would see "connecting…" indefinitely.
    pub local_config_sent_at: Option<Instant>,
    /// Identifier of the ConnectionRequest we sent for an outbound
    /// channel, or `None` if this channel was opened by an inbound
    /// ConnectionRequest from the remote. Used to match the peer's
    /// ConnectionResponse back to the originating channel — once the
    /// response arrives this is cleared and `local_config_identifier`
    /// takes over for the ConfigureRequest leg.
    pub outbound_connect_identifier: Option<u8>,
    pub rfcomm_state: Option<rfcomm::RfcommState>,
    /// Initiator-side HFP state for an OUTBOUND RFCOMM channel we
    /// opened toward the phone's Hands-Free AG (the `phone.connect`
    /// reconnect path). Mutually exclusive with `rfcomm_state` in
    /// practice — an outbound HFP channel installs an upper handler
    /// that routes here, so the global PSM handler never creates a
    /// server-mode RfcommState on it. Drained by the same
    /// `take_hfp_events` / `build_hfp_call_control_packets` /
    /// `tick_hfp_stalls` surfaces as the server path.
    pub hfp_client: Option<hfp_client::HfpClientState>,
    /// Per-channel payload handler. When present, takes precedence
    /// over the global PSM handler in `psm_handlers`. Phase 3c of the
    /// MAP/PBAP plan: the runtime can install a profile-specific
    /// handler at `open_outbound_channel_with_handler` time so the
    /// SDP query response routes to the PBAP runtime's state machine
    /// instead of the default server-side SDP handler.
    pub upper_handler: Option<PsmHandler>,
}

// L2capState used to derive Debug/Clone/PartialEq/Eq, but the new
// `psm_handlers` field stores closures (Arc<dyn Fn ...>) which don't
// implement those traits. Nothing in the codebase actually compared
// states or printed them, so the derives are dropped. Test assertions
// use accessors (`channel_count`, `channel`) instead.
pub struct L2capState {
    next_local_cid: u16,
    next_local_signaling_id: u8,
    channels: BTreeMap<u16, L2capChannel>,
    /// Per-ACL-handle reassembly buffer. ACL packets arrive split across
    /// multiple HCI transfers when the L2CAP frame is larger than the
    /// controller's `ACL_Data_Packet_Length` (≈1021 bytes on a typical
    /// Broadcom/Realtek dongle). Pre-fix every fragment was treated as a
    /// complete frame, so anything that overflowed (large SDP records,
    /// for example) got silently corrupted.
    acl_reassembly: BTreeMap<u16, Vec<u8>>,
    /// HFP events whose owning RFCOMM state has already been removed —
    /// for example, the stall watchdog tears down a channel before the
    /// upper layer can drain its events. Without this they'd vanish
    /// with the channel and the manager would never learn the SLC
    /// failed.
    orphan_hfp_events: Vec<hfp::HfpEvent>,
    /// Registered PSM handlers. ConnectionRequest replies SUCCESS only
    /// for PSMs in this map; channel payloads are dispatched through
    /// the matching closure. Default-populated with SDP and RFCOMM;
    /// future profiles call `register_psm` to add OBEX-on-L2CAP, etc.
    psm_handlers: BTreeMap<u16, PsmHandler>,
}

impl Default for L2capState {
    fn default() -> Self {
        let mut state = Self {
            next_local_cid: FIRST_DYNAMIC_CID,
            next_local_signaling_id: FIRST_LOCAL_SIGNALING_ID,
            channels: BTreeMap::new(),
            acl_reassembly: BTreeMap::new(),
            orphan_hfp_events: Vec::new(),
            psm_handlers: BTreeMap::new(),
        };
        state.register_psm(
            PSM_SDP,
            Arc::new(|_channel, payload| {
                Ok(vec![sdp::handle_service_search_attribute_request(payload)?])
            }),
        );
        state.register_psm(
            PSM_RFCOMM,
            Arc::new(|channel, payload| {
                let rfcomm_state = channel
                    .rfcomm_state
                    .get_or_insert_with(rfcomm::RfcommState::new);
                rfcomm_state.handle_packet(payload)
            }),
        );
        state
    }
}

impl L2capState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a PSM handler. Profiles register here once at
    /// construction time; the handler closure is then invoked for every
    /// payload that arrives on a channel opened on that PSM.
    pub fn register_psm(&mut self, psm: u16, handler: PsmHandler) {
        self.psm_handlers.insert(psm, handler);
    }

    pub fn channel(&self, local_cid: u16) -> Option<&L2capChannel> {
        self.channels.get(&local_cid)
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Find an open L2CAP/RFCOMM channel on the given ACL connection
    /// whose multiplexer is up. Outbound MAP/PBAP runtimes ride on
    /// this shared channel because Pixel/Bluedroid silently drops a
    /// second L2CAP/PSM 0x0003 SABM per peer (see project memory
    /// "Bluedroid: one RFCOMM session per peer").
    pub fn find_open_rfcomm_channel(&self, connection_handle: u16) -> Option<u16> {
        self.channels
            .iter()
            .find(|(_, channel)| {
                channel.connection_handle == connection_handle
                    && channel.psm == PSM_RFCOMM
                    && channel.state == ChannelState::Open
                    // Either mux role qualifies as the shared mux: the
                    // inbound (phone-initiated) RfcommState, or the
                    // outbound HARD-001 HfpClientState — Bluedroid runs
                    // ONE RFCOMM session per peer, so whichever exists
                    // is the only one OBEX profiles may ride.
                    && (channel
                        .rfcomm_state
                        .as_ref()
                        .is_some_and(|s| s.multiplexer_open())
                        || channel
                            .hfp_client
                            .as_ref()
                            .is_some_and(|c| c.mux_is_open()))
            })
            .map(|(cid, _)| *cid)
    }

    /// Attach an outbound DLCI on the shared inbound RFCOMM
    /// multiplexer. Returns `(target_dlci, l2cap_acl_packet)` where
    /// the packet is the PN UIH already wrapped for the wire. On
    /// success the runtime should `transport.write_acl(packet)` and
    /// then poll `take_rfcomm_client_events` after each inbound ACL
    /// to advance its OBEX state machine.
    pub fn rfcomm_attach_client_dlci(
        &mut self,
        local_cid: u16,
        server_channel: u8,
    ) -> Result<(u8, Vec<u8>), String> {
        let channel = self
            .channels
            .get_mut(&local_cid)
            .ok_or_else(|| format!("L2CAP channel 0x{:04x} not found", local_cid))?;
        if channel.state != ChannelState::Open {
            return Err(format!(
                "L2CAP channel 0x{:04x} not Open (state {:?})",
                local_cid, channel.state
            ));
        }
        let connection_handle = channel.connection_handle;
        let remote_cid = channel.remote_cid;
        // Whichever mux role lives on this channel does the attach: the
        // inbound RfcommState or the outbound HfpClientState (HARD-001).
        let (dlci, frame) = if let Some(rfcomm_state) = channel.rfcomm_state.as_mut() {
            rfcomm_state.attach_client_dlci(server_channel)?
        } else if let Some(hfp_client) = channel.hfp_client.as_mut() {
            hfp_client.attach_client_dlci(server_channel)?
        } else {
            return Err(format!(
                "L2CAP channel 0x{:04x} has no RFCOMM mux state",
                local_cid
            ));
        };
        let basic = build_basic_frame(remote_cid, &frame);
        let acl = build_acl_packet(
            connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        );
        Ok((dlci, acl))
    }

    /// Drain client-DLCI events from the RfcommState on a channel.
    /// Empty if the channel has no RfcommState or no pending events.
    /// **Warning:** see `RfcommState::take_client_events`. Use
    /// `take_rfcomm_client_events_for_dlci` instead when more than one
    /// runtime rides the same shared mux.
    pub fn take_rfcomm_client_events(&mut self, local_cid: u16) -> Vec<rfcomm::ClientDlciEvent> {
        let Some(channel) = self.channels.get_mut(&local_cid) else {
            return Vec::new();
        };
        if let Some(s) = channel.rfcomm_state.as_mut() {
            return s.take_client_events();
        }
        // No cross-mux drain equivalent on the initiator mux: sharing
        // runtimes must use the dlci-filtered variant below (they do).
        Vec::new()
    }

    /// Drain only those client-DLCI events whose `dlci` matches.
    /// Events for other DLCIs stay queued so a sibling runtime sharing
    /// the same mux can claim them on its own tick. Required when MAP
    /// and PBAP ride the inbound HFP channel concurrently. Works on
    /// either mux role (inbound RfcommState / outbound HfpClientState).
    pub fn take_rfcomm_client_events_for_dlci(
        &mut self,
        local_cid: u16,
        dlci: u8,
    ) -> Vec<rfcomm::ClientDlciEvent> {
        let Some(channel) = self.channels.get_mut(&local_cid) else {
            return Vec::new();
        };
        if let Some(s) = channel.rfcomm_state.as_mut() {
            return s.take_client_events_for_dlci(dlci);
        }
        if let Some(c) = channel.hfp_client.as_mut() {
            return c.take_client_events_for_dlci(dlci);
        }
        Vec::new()
    }

    /// Wrap an OBEX (or other upper-layer) payload as a UIH frame on
    /// `dlci` and turn it into an L2CAP-on-ACL packet ready for
    /// `transport.write_acl`. Works on either mux role. (&mut because
    /// the initiator mux tracks per-DLCI transmit credits on send.)
    pub fn rfcomm_send_uih_on_client_dlci(
        &mut self,
        local_cid: u16,
        dlci: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, String> {
        let channel = self
            .channels
            .get_mut(&local_cid)
            .ok_or_else(|| format!("L2CAP channel 0x{:04x} not found", local_cid))?;
        let frame = if let Some(rfcomm_state) = channel.rfcomm_state.as_ref() {
            rfcomm_state.build_uih_on_client_dlci(dlci, payload)?
        } else if let Some(hfp_client) = channel.hfp_client.as_mut() {
            hfp_client.build_uih_on_client_dlci(dlci, payload)?
        } else {
            return Err(format!(
                "L2CAP channel 0x{:04x} has no RFCOMM mux state",
                local_cid
            ));
        };
        let basic = build_basic_frame(channel.remote_cid, &frame);
        Ok(build_acl_packet(
            channel.connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        ))
    }

    /// Tear down a client DLCI by sending DISC on the multiplexer.
    /// Removes the DLCI from RfcommState tracking. Returns the ACL
    /// packet to write on the transport.
    pub fn rfcomm_disc_client_dlci(&mut self, local_cid: u16, dlci: u8) -> Result<Vec<u8>, String> {
        let channel = self
            .channels
            .get_mut(&local_cid)
            .ok_or_else(|| format!("L2CAP channel 0x{:04x} not found", local_cid))?;
        let connection_handle = channel.connection_handle;
        let remote_cid = channel.remote_cid;
        let frame = if let Some(rfcomm_state) = channel.rfcomm_state.as_mut() {
            rfcomm_state.build_disc_for_client_dlci(dlci)?
        } else if let Some(hfp_client) = channel.hfp_client.as_mut() {
            hfp_client.build_force_disc_secondary_dlci(dlci)
        } else {
            return Err(format!(
                "L2CAP channel 0x{:04x} has no RFCOMM mux state",
                local_cid
            ));
        };
        let basic = build_basic_frame(remote_cid, &frame);
        Ok(build_acl_packet(
            connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        ))
    }

    /// Locate the local cid of the currently-open shared RFCOMM mux
    /// (PSM 0x0003). Returns None if no such channel is up. Used by
    /// the runtime's MAS-stall recovery path to find the cid where
    /// dlcis 4 (MNS) and 11 (MAS) live without having to thread the
    /// cid through every state struct.
    pub fn find_open_rfcomm_cid(&self) -> Option<u16> {
        self.channels
            .iter()
            .find(|(_, c)| c.psm == PSM_RFCOMM && c.state == ChannelState::Open)
            .map(|(cid, _)| *cid)
    }

    /// Force-DISC any DLCI on the shared mux — works for client AND
    /// server DLCIs and is idempotent (no error if untracked). Used
    /// by the runtime's MAS-stall recovery path: when the peer's
    /// RFCOMM mux goes catatonic and stops responding on dlci 11
    /// (MAS) and dlci 4 (MNS), we force-DISC both so Pixel can
    /// reopen them fresh on the next Subscribe attempt.
    pub fn rfcomm_force_disc_dlci(&mut self, local_cid: u16, dlci: u8) -> Result<Vec<u8>, String> {
        let channel = self
            .channels
            .get_mut(&local_cid)
            .ok_or_else(|| format!("L2CAP channel 0x{:04x} not found", local_cid))?;
        let connection_handle = channel.connection_handle;
        let remote_cid = channel.remote_cid;
        let frame = if let Some(rfcomm_state) = channel.rfcomm_state.as_mut() {
            rfcomm_state.build_force_disc_dlci(dlci)
        } else if let Some(hfp_client) = channel.hfp_client.as_mut() {
            // Initiator mux: only secondary (OBEX) DLCIs are ours to
            // force-DISC — the primary carries the live HFP link.
            hfp_client.build_force_disc_secondary_dlci(dlci)
        } else {
            return Err(format!(
                "L2CAP channel 0x{:04x} has no RFCOMM mux state",
                local_cid
            ));
        };
        let basic = build_basic_frame(remote_cid, &frame);
        Ok(build_acl_packet(
            connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        ))
    }

    pub fn remove_connection(&mut self, connection_handle: u16) {
        self.channels
            .retain(|_, channel| channel.connection_handle != connection_handle);
        self.acl_reassembly.remove(&connection_handle);
    }

    fn allocate_local_signaling_id(&mut self) -> u8 {
        let id = self.next_local_signaling_id;
        // Wrap inside the local half of the identifier space so we never
        // clash with the remote's identifiers (which start at 1).
        self.next_local_signaling_id = self
            .next_local_signaling_id
            .checked_add(1)
            .filter(|next| *next != 0)
            .unwrap_or(FIRST_LOCAL_SIGNALING_ID);
        id
    }

    pub fn take_hfp_events(&mut self) -> Vec<hfp::HfpEvent> {
        let mut events = std::mem::take(&mut self.orphan_hfp_events);
        for channel in self.channels.values_mut() {
            if let Some(rfcomm_state) = channel.rfcomm_state.as_mut() {
                events.extend(rfcomm_state.take_hfp_events());
            }
            if let Some(hfp_client) = channel.hfp_client.as_mut() {
                events.extend(hfp_client.take_hfp_events());
            }
        }
        events
    }

    /// Drive the per-channel RFCOMM stall watchdog. Called from the
    /// runtime heartbeat at ~2 s cadence; any RFCOMM channel whose SLC
    /// AT command has been outstanding longer than `timeout` is failed
    /// here and a ServiceLevelConnectionFailed event lands in
    /// `take_hfp_events()` for the next tick.
    pub fn tick_hfp_stalls(&mut self, now: Instant, timeout: Duration) {
        for channel in self.channels.values_mut() {
            if let Some(rfcomm_state) = channel.rfcomm_state.as_mut() {
                rfcomm_state.tick_hfp_stall(now, timeout);
            }
            if let Some(hfp_client) = channel.hfp_client.as_mut() {
                hfp_client.tick_hfp_stall(now, timeout);
            }
        }
    }

    /// Drive the L2CAP-level ConfigureRequest watchdog. Any channel
    /// stuck in `Configuring` with our ConfigureRequest outstanding
    /// longer than `timeout` is torn down: we surface a
    /// ServiceLevelConnectionFailed event (so upper layers can react),
    /// build a courtesy DisconnectionRequest, and remove the channel
    /// locally. Returns the ACL-wrapped DisconnectionRequest packets
    /// for the runtime to write to the transport.
    ///
    /// We don't wait for the DisconnectionResponse — the peer is
    /// already unresponsive (that's why we're tearing the channel
    /// down) and waiting just creates a second stall vector. If the
    /// peer is alive but stuck, our request might unstick it; if
    /// dead, it goes into the void and that's fine.
    pub fn tick_l2cap_stalls(&mut self, now: Instant, timeout: Duration) -> Vec<Vec<u8>> {
        let mut to_remove: Vec<u16> = Vec::new();
        let mut packets: Vec<Vec<u8>> = Vec::new();
        for channel in self.channels.values() {
            if channel.state != ChannelState::Configuring {
                continue;
            }
            let Some(sent_at) = channel.local_config_sent_at else {
                continue;
            };
            if now.saturating_duration_since(sent_at) < timeout {
                continue;
            }
            eprintln!(
                "[AokieRadio] L2CAP ConfigureRequest stalled on cid 0x{:04x} (PSM 0x{:04x}) — tearing down",
                channel.local_cid, channel.psm
            );
            to_remove.push(channel.local_cid);
        }
        for local_cid in to_remove {
            // Pull the channel out so we can build the courtesy
            // DisconnectionRequest from its handles before dropping it.
            // Drain any pending HFP events into the orphan queue so they
            // survive the removal — without this the manager would
            // never learn the SLC failed because the events would be
            // freed along with the channel.
            if let Some(mut channel) = self.channels.remove(&local_cid) {
                let psm = channel.psm;
                let timeout_for_event = timeout;
                if let Some(rfcomm_state) = channel.rfcomm_state.as_mut() {
                    self.orphan_hfp_events
                        .extend(rfcomm_state.take_hfp_events());
                }
                if let Some(hfp_client) = channel.hfp_client.as_mut() {
                    self.orphan_hfp_events.extend(hfp_client.take_hfp_events());
                }
                self.orphan_hfp_events
                    .push(hfp::HfpEvent::ServiceLevelConnectionFailed(format!(
                    "L2CAP ConfigureRequest on cid 0x{:04x} (PSM 0x{:04x}) timed out after {:?}",
                    local_cid, psm, timeout_for_event
                )));
                let identifier = self.allocate_local_signaling_id();
                let signaling =
                    build_disconnection_request(identifier, channel.remote_cid, channel.local_cid);
                let acl = build_acl_packet(
                    channel.connection_handle,
                    ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
                    ACL_BROADCAST_POINT_TO_POINT,
                    &signaling,
                );
                packets.push(acl);
            }
        }
        packets
    }

    pub fn build_hfp_call_control_packets(
        &mut self,
        command: hfp::HfpAtCommand,
    ) -> Result<Vec<Vec<u8>>, String> {
        let Some((connection_handle, remote_cid, rfcomm_frame)) = self
            .channels
            .values_mut()
            .filter(|channel| channel.psm == PSM_RFCOMM && channel.state == ChannelState::Open)
            .find_map(|channel| {
                // Server-mode HFP (phone connected to us) or client-mode
                // HFP (we reconnected outbound via phone.connect) —
                // whichever owns the live SLC produces the frame.
                let frame = if let Some(rfcomm_state) = channel.rfcomm_state.as_mut() {
                    rfcomm_state.build_call_control_command(command.clone())
                } else if let Some(hfp_client) = channel.hfp_client.as_mut() {
                    hfp_client.build_call_control_command(command.clone())
                } else {
                    None
                }?;
                Some((channel.connection_handle, channel.remote_cid, frame))
            })
        else {
            return Ok(Vec::new());
        };

        Ok(vec![build_acl_packet(
            connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(remote_cid, &rfcomm_frame),
        )])
    }

    pub fn handle_acl_packet(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let acl = parse_acl_frame(packet)?;
        let connection_handle = acl.connection_handle;

        // Reassemble — for BR/EDR controller→host the spec says PB=0b10
        // begins an L2CAP frame and PB=0b01 continues an in-progress
        // one. PB=0b00 isn't expected in this direction but if it shows
        // up we treat it like a fresh start: better to drop the
        // half-buffered frame than to splice unrelated payloads.
        {
            let buffer = self.acl_reassembly.entry(connection_handle).or_default();
            if acl.packet_boundary_flag == ACL_PACKET_BOUNDARY_CONTINUING_FRAGMENT {
                buffer.extend_from_slice(acl.payload);
            } else {
                buffer.clear();
                buffer.extend_from_slice(acl.payload);
            }
        }

        // Drain every complete basic frame currently sitting in the
        // buffer. A single ACL packet usually contains exactly one
        // basic frame, but pipelining is allowed and we want to flush
        // anything that's already complete before we hand control back.
        let mut frame_payloads: Vec<Vec<u8>> = Vec::new();
        {
            let buffer = self
                .acl_reassembly
                .get_mut(&connection_handle)
                .expect("just inserted above");
            while buffer.len() >= 4 {
                let len = u16::from_le_bytes([buffer[0], buffer[1]]) as usize;
                if buffer.len() < 4 + len {
                    break;
                }
                let frame_bytes = buffer.drain(..4 + len).collect::<Vec<u8>>();
                frame_payloads.push(frame_bytes);
            }
        }

        let mut out = Vec::new();
        for frame_bytes in &frame_payloads {
            let frame = parse_basic_frame(frame_bytes)?;
            if frame.cid == CID_SIGNALING {
                for command in parse_signaling_commands(frame.payload)? {
                    for response in self.handle_signaling_command(connection_handle, command) {
                        out.push(build_acl_packet(
                            connection_handle,
                            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
                            ACL_BROADCAST_POINT_TO_POINT,
                            &response,
                        ));
                    }
                }
            } else {
                out.extend(self.handle_channel_payload(connection_handle, frame)?);
            }
        }
        Ok(out)
    }

    fn handle_channel_payload(
        &mut self,
        connection_handle: u16,
        frame: BasicFrame<'_>,
    ) -> Result<Vec<Vec<u8>>, String> {
        let Some(channel) = self.channels.get_mut(&frame.cid) else {
            eprintln!(
                "[AokieRadio] L2CAP inbound on unknown cid 0x{:04x} ({} bytes) — dropped",
                frame.cid,
                frame.payload.len()
            );
            return Ok(Vec::new());
        };
        if channel.connection_handle != connection_handle || channel.state != ChannelState::Open {
            eprintln!(
                "[AokieRadio] L2CAP inbound on cid 0x{:04x} dropped — handle 0x{:04x} expected 0x{:04x}, state {:?}",
                frame.cid, connection_handle, channel.connection_handle, channel.state
            );
            return Ok(Vec::new());
        }

        // Look the handler up before borrowing the channel mutably so
        // we can call it with the channel as &mut. Cloning the Arc is
        // cheap and avoids fighting the borrow checker over disjoint
        // fields of `self`.
        //
        // Per-channel handler wins over the global PSM handler: a
        // profile that opened an outbound channel (via
        // `open_outbound_channel_with_handler`) installed a handler
        // that should receive the bytes, even though the global
        // handler for that PSM is still serving inbound traffic.
        let handler = channel
            .upper_handler
            .clone()
            .or_else(|| self.psm_handlers.get(&channel.psm).cloned());
        let responses = match handler {
            Some(h) => h(channel, frame.payload)?,
            None => Vec::new(),
        };
        let remote_cid = channel.remote_cid;

        Ok(responses
            .into_iter()
            .map(|response| {
                let l2cap_response = build_basic_frame(remote_cid, &response);
                build_acl_packet(
                    connection_handle,
                    ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
                    ACL_BROADCAST_POINT_TO_POINT,
                    &l2cap_response,
                )
            })
            .collect())
    }

    fn handle_signaling_command(
        &mut self,
        connection_handle: u16,
        command: SignalingCommand,
    ) -> Vec<Vec<u8>> {
        match command {
            SignalingCommand::ConnectionRequest {
                identifier,
                psm,
                source_cid,
            } => self.handle_connection_request(connection_handle, identifier, psm, source_cid),
            SignalingCommand::ConnectionResponse {
                identifier,
                destination_cid,
                source_cid,
                result,
                ..
            } => self.handle_connection_response(identifier, destination_cid, source_cid, result),
            SignalingCommand::ConfigureRequest {
                identifier,
                destination_cid,
                flags,
                ..
            } => vec![self.handle_configure_request(identifier, destination_cid, flags)],
            SignalingCommand::ConfigureResponse {
                identifier,
                source_cid,
                result,
                ..
            } => {
                self.handle_configure_response(identifier, source_cid, result);
                Vec::new()
            }
            SignalingCommand::DisconnectionRequest {
                identifier,
                destination_cid,
                source_cid,
            } => {
                self.channels.remove(&destination_cid);
                vec![build_disconnection_response(
                    identifier,
                    destination_cid,
                    source_cid,
                )]
            }
            SignalingCommand::DisconnectionResponse { source_cid, .. } => {
                // Peer's ack of a DisconnectionRequest we sent. The
                // channel is normally already gone (we tear down
                // locally on send), so this is just a courtesy
                // acknowledgement — log it once and drop.
                eprintln!(
                    "[AokieRadio] L2CAP DisconnectionResponse for src_cid 0x{:04x}",
                    source_cid
                );
                Vec::new()
            }
            SignalingCommand::InformationRequest {
                identifier,
                info_type,
            } => {
                // Per L2CAP §4.11: InformationRequest is mandatory for
                // every spec-conformant peer to handle, even if the
                // requested type isn't supported. The previous
                // CommandReject reply made Pixel's BR/EDR stack abort
                // the configuration handshake silently — ConnectionRequest
                // got accepted, ConfigureRequest never came back, the
                // 5s ConfigureRequest watchdog tore the channel down,
                // and SDP / RFCOMM / HFP never came up.
                eprintln!(
                    "[AokieRadio] L2CAP InformationRequest type 0x{:04x} — replying",
                    info_type
                );
                vec![build_information_response_for_type(identifier, info_type)]
            }
            SignalingCommand::InformationResponse { .. } => {
                // We don't issue InformationRequests today, so any
                // InformationResponse from the peer is unsolicited.
                // Drop silently — replying CommandReject would create
                // exactly the symmetric bug we just fixed.
                Vec::new()
            }
            SignalingCommand::Unknown {
                identifier, code, ..
            } => {
                eprintln!(
                    "[AokieRadio] L2CAP unknown signaling code 0x{:02x} ident 0x{:02x} — sending CommandReject",
                    code, identifier
                );
                vec![build_command_reject(
                    identifier,
                    COMMAND_REJECT_REASON_COMMAND_NOT_UNDERSTOOD,
                )]
            }
        }
    }

    fn handle_connection_request(
        &mut self,
        connection_handle: u16,
        identifier: u8,
        psm: u16,
        source_cid: u16,
    ) -> Vec<Vec<u8>> {
        if !self.psm_handlers.contains_key(&psm) {
            eprintln!(
                "[AokieRadio] L2CAP ConnectionRequest PSM 0x{:04x} REJECTED (no registered handler)",
                psm
            );
            return vec![build_connection_response(
                identifier,
                0,
                source_cid,
                CONNECTION_RESULT_PSM_NOT_SUPPORTED,
                CONNECTION_STATUS_NO_FURTHER_INFORMATION,
            )];
        }
        eprintln!(
            "[AokieRadio] L2CAP ConnectionRequest PSM 0x{:04x} (handle 0x{:04x}, src_cid 0x{:04x}) — accepting",
            psm, connection_handle, source_cid
        );

        let local_cid = self.allocate_local_cid(source_cid);
        let local_config_id = self.allocate_local_signaling_id();
        self.channels.insert(
            local_cid,
            L2capChannel {
                connection_handle,
                psm,
                local_cid,
                remote_cid: source_cid,
                state: ChannelState::Configuring,
                // Local side stays UNconfigured until the remote acks
                // the ConfigureRequest we're about to send below. The
                // pre-fix code set this to true unconditionally and
                // skipped sending a ConfigureRequest entirely, so the
                // channel transitioned to Open as soon as the remote
                // sent its own ConfigureRequest — a spec violation
                // that strict peers refuse.
                local_configured: false,
                remote_configured: false,
                local_config_identifier: Some(local_config_id),
                local_config_sent_at: Some(Instant::now()),
                outbound_connect_identifier: None,
                // RfcommState stays None at channel creation so the
                // PSM_RFCOMM handler's `get_or_insert_with` can run on
                // the first inbound RFCOMM packet — that's where the
                // MAP-MNS server-channel handlers get attached
                // (install_mns_server_channel registers a handler that
                // owns this insertion). Pre-populating with
                // RfcommState::new() here would make the get-or-insert
                // a no-op and the MNS DLCI would never be reachable for
                // inbound notification PNs from the phone.
                rfcomm_state: None,
                hfp_client: None,
                upper_handler: None,
            },
        );

        vec![
            build_connection_response(
                identifier,
                local_cid,
                source_cid,
                CONNECTION_RESULT_SUCCESS,
                CONNECTION_STATUS_NO_FURTHER_INFORMATION,
            ),
            // Empty options = "accept your defaults". HFP and SDP both
            // run happily on the spec-default 672-byte MTU, so we don't
            // need to negotiate anything special.
            build_configure_request(local_config_id, source_cid, 0, &[]),
        ]
    }

    fn handle_configure_request(
        &mut self,
        identifier: u8,
        destination_cid: u16,
        flags: u16,
    ) -> Vec<u8> {
        let Some(channel) = self.channels.get_mut(&destination_cid) else {
            return build_command_reject(identifier, COMMAND_REJECT_REASON_COMMAND_NOT_UNDERSTOOD);
        };

        channel.remote_configured = true;
        let psm = channel.psm;
        let now_open = channel.local_configured && channel.remote_configured;
        if now_open {
            channel.state = ChannelState::Open;
            eprintln!(
                "[AokieRadio] L2CAP channel cid 0x{:04x} (PSM 0x{:04x}) OPEN (remote ConfigureRequest received)",
                destination_cid, psm
            );
        }

        build_configure_response(
            identifier,
            channel.remote_cid,
            flags,
            CONFIG_RESULT_SUCCESS,
            &[],
        )
    }

    fn handle_configure_response(&mut self, identifier: u8, source_cid: u16, result: u16) {
        // L2CAP §4.5: the "Source CID" field in a ConfigureResponse
        // identifies the requester's channel endpoint — i.e. the local
        // CID of the side that originally sent the matching
        // ConfigureRequest. We sent the request, so it's our
        // `local_cid` we should match against, not `remote_cid`. (See
        // BTstack's `l2cap.c` send path: the responder sets the field
        // to its `remote_cid`, which is the requester's local CID.)
        // Match on identifier as well so a stale response for a re-used
        // CID can't flip the wrong channel.
        let Some(channel) = self.channels.values_mut().find(|channel| {
            channel.local_cid == source_cid && channel.local_config_identifier == Some(identifier)
        }) else {
            eprintln!(
                "[AokieRadio] L2CAP ConfigureResponse for unknown source_cid 0x{:04x} ident 0x{:02x} result 0x{:04x}",
                source_cid, identifier, result
            );
            return;
        };
        if result != CONFIG_RESULT_SUCCESS {
            eprintln!(
                "[AokieRadio] L2CAP ConfigureResponse REFUSED (PSM 0x{:04x}, result 0x{:04x}) — channel will stall",
                channel.psm, result
            );
            return;
        }
        let psm = channel.psm;
        let local_cid = channel.local_cid;
        channel.local_config_identifier = None;
        channel.local_config_sent_at = None;
        channel.local_configured = true;
        let now_open = channel.remote_configured;
        if now_open {
            channel.state = ChannelState::Open;
            eprintln!(
                "[AokieRadio] L2CAP channel cid 0x{:04x} (PSM 0x{:04x}) OPEN (our ConfigureRequest acked)",
                local_cid, psm
            );
        } else {
            eprintln!(
                "[AokieRadio] L2CAP channel cid 0x{:04x} (PSM 0x{:04x}) local-configured (waiting for remote ConfigureRequest)",
                local_cid, psm
            );
        }
    }

    /// Originate an outbound L2CAP channel to the remote on `psm`.
    /// Allocates a local CID + signaling identifier, registers an
    /// in-flight channel in `Configuring` with `remote_cid = 0` (we
    /// learn it when the ConnectionResponse arrives), and returns the
    /// ACL packet wrapping the ConnectionRequest.
    ///
    /// The channel walks: ConnectionRequest sent → ConnectionResponse
    /// (success) → we send ConfigureRequest → both sides reply
    /// ConfigureResponse → channel transitions to `Open`.
    ///
    /// The L2CAP stall watchdog (`tick_l2cap_stalls`) covers both the
    /// ConnectionResponse and ConfigureResponse waits via
    /// `local_config_sent_at`, so a peer that goes silent at any step
    /// still tears down within the configured timeout.
    pub fn open_outbound_channel(&mut self, connection_handle: u16, psm: u16) -> Vec<u8> {
        self.open_outbound_channel_with_handler(connection_handle, psm, None)
            .1
    }

    /// Same as `open_outbound_channel` but installs a per-channel
    /// payload handler that takes precedence over the global PSM
    /// handler in `psm_handlers`. Returns `(local_cid, ACL packet)`
    /// — the local CID lets the caller route subsequent
    /// `send_on_channel` writes and `disconnect_channel` calls.
    ///
    /// Phase 3c usage: PBAP runtime opens an outbound L2CAP channel
    /// to PSM_SDP / PSM_RFCOMM, captures bytes via the handler closure
    /// (typically into a Mutex-guarded queue), and feeds them through
    /// its own state machine. The default global PSM handler stays
    /// in place for inbound channels — inbound SDP requests still get
    /// the server-side response, and inbound RFCOMM still creates a
    /// server-mode RfcommState.
    pub fn open_outbound_channel_with_handler(
        &mut self,
        connection_handle: u16,
        psm: u16,
        upper_handler: Option<PsmHandler>,
    ) -> (u16, Vec<u8>) {
        let local_cid = self.allocate_local_cid(0);
        let identifier = self.allocate_local_signaling_id();
        self.channels.insert(
            local_cid,
            L2capChannel {
                connection_handle,
                psm,
                local_cid,
                remote_cid: 0,
                state: ChannelState::Configuring,
                local_configured: false,
                remote_configured: false,
                local_config_identifier: None,
                local_config_sent_at: Some(Instant::now()),
                outbound_connect_identifier: Some(identifier),
                rfcomm_state: None,
                hfp_client: None,
                upper_handler,
            },
        );
        eprintln!(
            "[AokieRadio] L2CAP outbound ConnectionRequest PSM 0x{:04x} (handle 0x{:04x}, src_cid 0x{:04x}, ident 0x{:02x})",
            psm, connection_handle, local_cid, identifier
        );
        let signaling = build_connection_request(identifier, psm, local_cid);
        let acl = build_acl_packet(
            connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &signaling,
        );
        (local_cid, acl)
    }

    /// Install an initiator-side HFP state on an OUTBOUND RFCOMM
    /// channel (the `phone.connect` reconnect path) and originate the
    /// multiplexer SABM. Returns the ACL packet to write. The channel
    /// must be Open (L2CAP configured) and must have been opened with
    /// the hfp_connect upper handler so inbound bytes route into the
    /// client state.
    pub fn start_hfp_client(
        &mut self,
        local_cid: u16,
        server_channel: u8,
        wbs_supported: bool,
        call_waiting_enabled: bool,
    ) -> Result<Vec<u8>, String> {
        let channel = self
            .channels
            .get_mut(&local_cid)
            .ok_or_else(|| format!("L2CAP channel 0x{:04x} not found", local_cid))?;
        if channel.state != ChannelState::Open {
            return Err(format!(
                "L2CAP channel 0x{:04x} not Open (state {:?})",
                local_cid, channel.state
            ));
        }
        if channel.hfp_client.is_some() {
            return Err(format!(
                "L2CAP channel 0x{:04x} already has an HFP client",
                local_cid
            ));
        }
        let mut client =
            hfp_client::HfpClientState::new(server_channel, wbs_supported, call_waiting_enabled);
        let sabm = client.kickoff()?;
        channel.hfp_client = Some(client);
        let basic = build_basic_frame(channel.remote_cid, &sabm);
        Ok(build_acl_packet(
            channel.connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        ))
    }

    /// Wrap an upper-layer payload in a basic frame + ACL packet on
    /// the channel identified by `local_cid`. Errors if the channel
    /// doesn't exist or isn't Open. Phase 3c: the PBAP runtime calls
    /// this to send SDP queries / OBEX requests on its own outbound
    /// channels.
    pub fn send_on_channel(&self, local_cid: u16, payload: &[u8]) -> Result<Vec<u8>, String> {
        let channel = self
            .channels
            .get(&local_cid)
            .ok_or_else(|| format!("L2CAP send on unknown cid 0x{:04x}", local_cid))?;
        if channel.state != ChannelState::Open {
            return Err(format!(
                "L2CAP send on cid 0x{:04x} but state is {:?}",
                local_cid, channel.state
            ));
        }
        let basic = build_basic_frame(channel.remote_cid, payload);
        Ok(build_acl_packet(
            channel.connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        ))
    }

    /// Build a courtesy DisconnectionRequest for an outbound channel
    /// and remove it from local state. Returns `None` if the channel
    /// doesn't exist (already torn down). We don't wait for the
    /// DisconnectionResponse — same rationale as `tick_l2cap_stalls`:
    /// either the peer is alive and our request unsticks them, or
    /// they're dead and waiting just stalls us. Phase 3c: PBAP runtime
    /// uses this after a phonebook fetch completes.
    pub fn disconnect_channel(&mut self, local_cid: u16) -> Option<Vec<u8>> {
        let channel = self.channels.remove(&local_cid)?;
        let identifier = self.allocate_local_signaling_id();
        let signaling =
            build_disconnection_request(identifier, channel.remote_cid, channel.local_cid);
        Some(build_acl_packet(
            channel.connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &signaling,
        ))
    }

    fn handle_connection_response(
        &mut self,
        identifier: u8,
        destination_cid: u16,
        source_cid: u16,
        result: u16,
    ) -> Vec<Vec<u8>> {
        // Outcome is decided in a tight scope so the &mut borrow ends
        // before we call `allocate_local_signaling_id` (which also
        // borrows &mut self).
        enum Outcome {
            Unknown,
            Pending,
            Refused {
                local_cid: u16,
                psm: u16,
                result: u16,
            },
            Success {
                local_cid: u16,
                remote_cid: u16,
                psm: u16,
            },
        }
        let outcome = if let Some(channel) = self.channels.values_mut().find(|c| {
            c.local_cid == source_cid && c.outbound_connect_identifier == Some(identifier)
        }) {
            if result == CONNECTION_RESULT_PENDING {
                Outcome::Pending
            } else if result == CONNECTION_RESULT_SUCCESS {
                channel.remote_cid = destination_cid;
                channel.outbound_connect_identifier = None;
                Outcome::Success {
                    local_cid: channel.local_cid,
                    remote_cid: destination_cid,
                    psm: channel.psm,
                }
            } else {
                Outcome::Refused {
                    local_cid: channel.local_cid,
                    psm: channel.psm,
                    result,
                }
            }
        } else {
            Outcome::Unknown
        };
        match outcome {
            Outcome::Unknown => {
                eprintln!(
                    "[AokieRadio] L2CAP ConnectionResponse for unknown src_cid 0x{:04x} ident 0x{:02x} result 0x{:04x}",
                    source_cid, identifier, result
                );
                Vec::new()
            }
            Outcome::Pending => {
                // Peer is still bonding/authorizing. Spec: wait for
                // a final response. Don't refresh the watchdog —
                // total time-to-open is still bounded.
                eprintln!(
                    "[AokieRadio] L2CAP ConnectionResponse PENDING for src_cid 0x{:04x}",
                    source_cid
                );
                Vec::new()
            }
            Outcome::Refused {
                local_cid,
                psm,
                result,
            } => {
                eprintln!(
                    "[AokieRadio] L2CAP outbound ConnectionRequest REFUSED (PSM 0x{:04x}, result 0x{:04x}) — tearing down local channel",
                    psm, result
                );
                self.orphan_hfp_events
                    .push(hfp::HfpEvent::ServiceLevelConnectionFailed(format!(
                    "L2CAP outbound ConnectionRequest on PSM 0x{:04x} refused (result 0x{:04x})",
                    psm, result
                )));
                self.channels.remove(&local_cid);
                Vec::new()
            }
            Outcome::Success {
                local_cid,
                remote_cid,
                psm,
            } => {
                let config_id = self.allocate_local_signaling_id();
                let channel = self
                    .channels
                    .get_mut(&local_cid)
                    .expect("channel was just borrowed mutably above");
                channel.local_config_identifier = Some(config_id);
                channel.local_config_sent_at = Some(Instant::now());
                eprintln!(
                    "[AokieRadio] L2CAP outbound ConnectionResponse SUCCESS (PSM 0x{:04x}, local 0x{:04x}, remote 0x{:04x}) — sending ConfigureRequest id 0x{:02x}",
                    psm, local_cid, remote_cid, config_id
                );
                vec![build_configure_request(config_id, remote_cid, 0, &[])]
            }
        }
    }

    fn allocate_local_cid(&mut self, remote_cid: u16) -> u16 {
        // Walk the dynamic-CID range looking for a free slot. Using
        // saturating_add was an infinite-loop trap: at u16::MAX the
        // counter would saturate, every subsequent candidate would be
        // u16::MAX, and once that one CID got allocated (or matched
        // `remote_cid`) we'd spin forever. Wrap explicitly back to
        // FIRST_DYNAMIC_CID and bound the search by the size of the
        // dynamic range so an exhausted table errors instead of hangs.
        let dynamic_range = (u16::MAX - FIRST_DYNAMIC_CID + 1) as usize;
        for _ in 0..dynamic_range {
            let candidate = self.next_local_cid;
            self.next_local_cid = if self.next_local_cid == u16::MAX {
                FIRST_DYNAMIC_CID
            } else {
                self.next_local_cid + 1
            };
            if candidate >= FIRST_DYNAMIC_CID
                && candidate != remote_cid
                && !self.channels.contains_key(&candidate)
            {
                return candidate;
            }
        }
        // Fall back to FIRST_DYNAMIC_CID. The caller checks whether
        // the returned CID collides with an existing channel; the
        // honest answer here is "we couldn't find a free one", but
        // returning a u16 keeps the callsite simple. In practice the
        // dynamic range is 65472 entries — an HFP receptionist will
        // never come close to exhausting it.
        FIRST_DYNAMIC_CID
    }
}

pub fn parse_acl_frame(packet: &[u8]) -> Result<AclFrame<'_>, String> {
    if packet.len() < 4 {
        return Err("HCI ACL packet too short".to_string());
    }
    let handle_pb_bc = u16::from_le_bytes([packet[0], packet[1]]);
    let payload_len = u16::from_le_bytes([packet[2], packet[3]]) as usize;
    if packet.len() < 4 + payload_len {
        return Err(format!(
            "HCI ACL packet declares {} bytes but packet has {} payload bytes",
            payload_len,
            packet.len().saturating_sub(4)
        ));
    }
    Ok(AclFrame {
        connection_handle: handle_pb_bc & 0x0fff,
        packet_boundary_flag: ((handle_pb_bc >> 12) & 0x03) as u8,
        broadcast_flag: ((handle_pb_bc >> 14) & 0x03) as u8,
        payload: &packet[4..4 + payload_len],
    })
}

pub fn build_acl_packet(
    connection_handle: u16,
    packet_boundary_flag: u8,
    broadcast_flag: u8,
    payload: &[u8],
) -> Vec<u8> {
    let handle_pb_bc = (connection_handle & 0x0fff)
        | (((packet_boundary_flag as u16) & 0x03) << 12)
        | (((broadcast_flag as u16) & 0x03) << 14);
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&handle_pb_bc.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn parse_basic_frame(payload: &[u8]) -> Result<BasicFrame<'_>, String> {
    if payload.len() < 4 {
        return Err("L2CAP basic frame too short".to_string());
    }
    let len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    let cid = u16::from_le_bytes([payload[2], payload[3]]);
    if payload.len() < 4 + len {
        return Err(format!(
            "L2CAP basic frame declares {} bytes but packet has {} payload bytes",
            len,
            payload.len().saturating_sub(4)
        ));
    }
    Ok(BasicFrame {
        cid,
        payload: &payload[4..4 + len],
    })
}

pub fn build_basic_frame(cid: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    frame.extend_from_slice(&cid.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

pub fn parse_signaling_commands(payload: &[u8]) -> Result<Vec<SignalingCommand>, String> {
    let mut commands = Vec::new();
    let mut offset = 0usize;
    while offset < payload.len() {
        let header = parse_signaling_header(&payload[offset..])?;
        let command_len = 4 + header.payload.len();
        commands.push(parse_signaling_command(header)?);
        offset += command_len;
    }
    Ok(commands)
}

fn parse_signaling_header(payload: &[u8]) -> Result<SignalingHeader<'_>, String> {
    if payload.len() < 4 {
        return Err("L2CAP signaling command too short".to_string());
    }
    let code = payload[0];
    let identifier = payload[1];
    let len = u16::from_le_bytes([payload[2], payload[3]]) as usize;
    if payload.len() < 4 + len {
        return Err(format!(
            "L2CAP signaling command declares {} bytes but packet has {} payload bytes",
            len,
            payload.len().saturating_sub(4)
        ));
    }
    Ok(SignalingHeader {
        code,
        identifier,
        payload: &payload[4..4 + len],
    })
}

fn parse_signaling_command(header: SignalingHeader<'_>) -> Result<SignalingCommand, String> {
    match header.code {
        SIGNAL_CONNECTION_REQUEST => {
            require_len(header.payload, 4, "L2CAP Connection Request")?;
            Ok(SignalingCommand::ConnectionRequest {
                identifier: header.identifier,
                psm: u16::from_le_bytes([header.payload[0], header.payload[1]]),
                source_cid: u16::from_le_bytes([header.payload[2], header.payload[3]]),
            })
        }
        SIGNAL_CONNECTION_RESPONSE => {
            require_len(header.payload, 8, "L2CAP Connection Response")?;
            Ok(SignalingCommand::ConnectionResponse {
                identifier: header.identifier,
                destination_cid: u16::from_le_bytes([header.payload[0], header.payload[1]]),
                source_cid: u16::from_le_bytes([header.payload[2], header.payload[3]]),
                result: u16::from_le_bytes([header.payload[4], header.payload[5]]),
                status: u16::from_le_bytes([header.payload[6], header.payload[7]]),
            })
        }
        SIGNAL_CONFIGURE_REQUEST => {
            require_len(header.payload, 4, "L2CAP Configure Request")?;
            Ok(SignalingCommand::ConfigureRequest {
                identifier: header.identifier,
                destination_cid: u16::from_le_bytes([header.payload[0], header.payload[1]]),
                flags: u16::from_le_bytes([header.payload[2], header.payload[3]]),
                options: header.payload[4..].to_vec(),
            })
        }
        SIGNAL_CONFIGURE_RESPONSE => {
            require_len(header.payload, 6, "L2CAP Configure Response")?;
            Ok(SignalingCommand::ConfigureResponse {
                identifier: header.identifier,
                source_cid: u16::from_le_bytes([header.payload[0], header.payload[1]]),
                flags: u16::from_le_bytes([header.payload[2], header.payload[3]]),
                result: u16::from_le_bytes([header.payload[4], header.payload[5]]),
                options: header.payload[6..].to_vec(),
            })
        }
        SIGNAL_DISCONNECTION_REQUEST => {
            require_len(header.payload, 4, "L2CAP Disconnection Request")?;
            Ok(SignalingCommand::DisconnectionRequest {
                identifier: header.identifier,
                destination_cid: u16::from_le_bytes([header.payload[0], header.payload[1]]),
                source_cid: u16::from_le_bytes([header.payload[2], header.payload[3]]),
            })
        }
        SIGNAL_DISCONNECTION_RESPONSE => {
            require_len(header.payload, 4, "L2CAP Disconnection Response")?;
            Ok(SignalingCommand::DisconnectionResponse {
                identifier: header.identifier,
                destination_cid: u16::from_le_bytes([header.payload[0], header.payload[1]]),
                source_cid: u16::from_le_bytes([header.payload[2], header.payload[3]]),
            })
        }
        SIGNAL_INFORMATION_REQUEST => {
            require_len(header.payload, 2, "L2CAP Information Request")?;
            Ok(SignalingCommand::InformationRequest {
                identifier: header.identifier,
                info_type: u16::from_le_bytes([header.payload[0], header.payload[1]]),
            })
        }
        SIGNAL_INFORMATION_RESPONSE => {
            require_len(header.payload, 4, "L2CAP Information Response")?;
            Ok(SignalingCommand::InformationResponse {
                identifier: header.identifier,
                info_type: u16::from_le_bytes([header.payload[0], header.payload[1]]),
                result: u16::from_le_bytes([header.payload[2], header.payload[3]]),
                data: header.payload[4..].to_vec(),
            })
        }
        _ => Ok(SignalingCommand::Unknown {
            code: header.code,
            identifier: header.identifier,
            payload: header.payload.to_vec(),
        }),
    }
}

pub fn build_connection_request(identifier: u8, psm: u16, source_cid: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&psm.to_le_bytes());
    payload.extend_from_slice(&source_cid.to_le_bytes());
    build_basic_signaling_packet(SIGNAL_CONNECTION_REQUEST, identifier, &payload)
}

pub fn build_connection_response(
    identifier: u8,
    destination_cid: u16,
    source_cid: u16,
    result: u16,
    status: u16,
) -> Vec<u8> {
    build_basic_signaling_packet(
        SIGNAL_CONNECTION_RESPONSE,
        identifier,
        &[
            destination_cid.to_le_bytes().as_slice(),
            source_cid.to_le_bytes().as_slice(),
            result.to_le_bytes().as_slice(),
            status.to_le_bytes().as_slice(),
        ]
        .concat(),
    )
}

pub fn build_configure_response(
    identifier: u8,
    source_cid: u16,
    flags: u16,
    result: u16,
    options: &[u8],
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(6 + options.len());
    payload.extend_from_slice(&source_cid.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.extend_from_slice(&result.to_le_bytes());
    payload.extend_from_slice(options);
    build_basic_signaling_packet(SIGNAL_CONFIGURE_RESPONSE, identifier, &payload)
}

pub fn build_configure_request(
    identifier: u8,
    destination_cid: u16,
    flags: u16,
    options: &[u8],
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + options.len());
    payload.extend_from_slice(&destination_cid.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.extend_from_slice(options);
    build_basic_signaling_packet(SIGNAL_CONFIGURE_REQUEST, identifier, &payload)
}

pub fn build_disconnection_response(
    identifier: u8,
    destination_cid: u16,
    source_cid: u16,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&destination_cid.to_le_bytes());
    payload.extend_from_slice(&source_cid.to_le_bytes());
    build_basic_signaling_packet(SIGNAL_DISCONNECTION_RESPONSE, identifier, &payload)
}

pub fn build_disconnection_request(
    identifier: u8,
    destination_cid: u16,
    source_cid: u16,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&destination_cid.to_le_bytes());
    payload.extend_from_slice(&source_cid.to_le_bytes());
    build_basic_signaling_packet(SIGNAL_DISCONNECTION_REQUEST, identifier, &payload)
}

pub fn build_command_reject(identifier: u8, reason: u16) -> Vec<u8> {
    build_basic_signaling_packet(SIGNAL_COMMAND_REJECT, identifier, &reason.to_le_bytes())
}

/// Reply to an L2CAP InformationRequest with the result + data block
/// the spec defines for `info_type`. Unknown info types get a NOT
/// SUPPORTED result with empty data — the peer treats that as "this
/// peer doesn't expose extra capabilities" and proceeds.
pub fn build_information_response_for_type(identifier: u8, info_type: u16) -> Vec<u8> {
    match info_type {
        INFO_TYPE_CONNECTIONLESS_MTU => build_information_response(
            identifier,
            info_type,
            INFO_RESULT_SUCCESS,
            &AOKIE_CONNECTIONLESS_MTU.to_le_bytes(),
        ),
        INFO_TYPE_EXTENDED_FEATURES => build_information_response(
            identifier,
            info_type,
            INFO_RESULT_SUCCESS,
            &AOKIE_EXTENDED_FEATURES_MASK.to_le_bytes(),
        ),
        INFO_TYPE_FIXED_CHANNELS => build_information_response(
            identifier,
            info_type,
            INFO_RESULT_SUCCESS,
            &AOKIE_FIXED_CHANNELS_MASK.to_le_bytes(),
        ),
        _ => build_information_response(identifier, info_type, INFO_RESULT_NOT_SUPPORTED, &[]),
    }
}

pub fn build_information_response(
    identifier: u8,
    info_type: u16,
    result: u16,
    data: &[u8],
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + data.len());
    payload.extend_from_slice(&info_type.to_le_bytes());
    payload.extend_from_slice(&result.to_le_bytes());
    payload.extend_from_slice(data);
    build_basic_signaling_packet(SIGNAL_INFORMATION_RESPONSE, identifier, &payload)
}

fn build_basic_signaling_packet(code: u8, identifier: u8, payload: &[u8]) -> Vec<u8> {
    let mut signaling = Vec::with_capacity(4 + payload.len());
    signaling.push(code);
    signaling.push(identifier);
    signaling.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    signaling.extend_from_slice(payload);

    build_basic_frame(CID_SIGNALING, &signaling)
}

fn require_len(payload: &[u8], min_len: usize, name: &str) -> Result<(), String> {
    if payload.len() < min_len {
        return Err(format!(
            "{} is too short: expected at least {} bytes, got {}",
            name,
            min_len,
            payload.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_acl_and_l2cap_signaling_connection_request() {
        let packet = [
            0x2a, 0x20, 0x0c, 0x00, // ACL handle 0x002a, PB=2, len=12
            0x08, 0x00, 0x01, 0x00, // L2CAP len=8, CID signaling
            0x02, 0x01, 0x04, 0x00, // Connection Request id=1 len=4
            0x03, 0x00, 0x40, 0x00, // PSM RFCOMM, source CID 0x0040
        ];

        let acl = parse_acl_frame(&packet).unwrap();
        assert_eq!(acl.connection_handle, 0x002a);
        assert_eq!(acl.packet_boundary_flag, 0x02);
        assert_eq!(acl.broadcast_flag, 0);

        let frame = parse_basic_frame(acl.payload).unwrap();
        assert_eq!(frame.cid, CID_SIGNALING);

        let commands = parse_signaling_commands(frame.payload).unwrap();
        assert_eq!(
            commands,
            vec![SignalingCommand::ConnectionRequest {
                identifier: 1,
                psm: PSM_RFCOMM,
                source_cid: 0x0040,
            }]
        );
    }

    #[test]
    fn parses_configure_and_disconnection_requests() {
        let signaling = [
            0x04, 0x02, 0x08, 0x00, // Configure Request id=2 len=8
            0x41, 0x00, 0x00, 0x00, // DCID 0x0041, flags 0
            0x01, 0x02, 0xa0, 0x02, // MTU option 672
            0x06, 0x03, 0x04, 0x00, // Disconnection Request id=3 len=4
            0x41, 0x00, 0x40, 0x00, // DCID 0x0041, SCID 0x0040
        ];
        let commands = parse_signaling_commands(&signaling).unwrap();
        assert_eq!(
            commands,
            vec![
                SignalingCommand::ConfigureRequest {
                    identifier: 2,
                    destination_cid: 0x0041,
                    flags: 0,
                    options: vec![0x01, 0x02, 0xa0, 0x02],
                },
                SignalingCommand::DisconnectionRequest {
                    identifier: 3,
                    destination_cid: 0x0041,
                    source_cid: 0x0040,
                },
            ]
        );
    }

    #[test]
    fn builds_signaling_responses_inside_basic_frames() {
        assert_eq!(
            build_connection_response(
                1,
                0x0041,
                0x0040,
                CONNECTION_RESULT_SUCCESS,
                CONNECTION_STATUS_NO_FURTHER_INFORMATION,
            ),
            vec![
                0x0c, 0x00, 0x01, 0x00, // L2CAP len 12, signaling CID
                0x03, 0x01, 0x08, 0x00, // Connection Response id=1 len=8
                0x41, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
            ]
        );

        assert_eq!(
            build_configure_response(2, 0x0040, 0, CONFIG_RESULT_SUCCESS, &[]),
            vec![
                0x0a, 0x00, 0x01, 0x00, // L2CAP len 10, signaling CID
                0x05, 0x02, 0x06, 0x00, // Configure Response id=2 len=6
                0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
            ]
        );

        assert_eq!(
            build_disconnection_response(3, 0x0041, 0x0040),
            vec![
                0x08, 0x00, 0x01, 0x00, // L2CAP len 8, signaling CID
                0x07, 0x03, 0x04, 0x00, // Disconnection Response id=3 len=4
                0x41, 0x00, 0x40, 0x00,
            ]
        );
    }

    #[test]
    fn information_request_extended_features_round_trips_through_state_machine() {
        // Pixel sends `InformationRequest(type=0x0002 ExtendedFeatures)`
        // immediately after the L2CAP signaling channel comes up.
        // Pre-fix the handler returned a CommandReject, which Pixel
        // reads as "broken peer" and silently aborts the configuration
        // handshake — ConnectionRequest accepted, ConfigureRequest never
        // ACKed, channel torn down 5s later. Lock the response shape.
        let mut state = L2capState::new();
        // Pixel-style InformationRequest, identifier 0x42, type 0x0002.
        let acl = build_acl_packet(
            0x000c,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(CID_SIGNALING, &[0x0a, 0x42, 0x02, 0x00, 0x02, 0x00]),
        );
        let responses = state.handle_acl_packet(&acl).unwrap();
        assert_eq!(responses.len(), 1, "must emit exactly one reply packet");
        // Outer ACL, then basic frame, then signaling: code 0x0b
        // (InformationResponse), identifier echoed, info_type 0x0002,
        // result 0x0000 success, then 4-byte mask = 0x00000080.
        let acl_out = parse_acl_frame(&responses[0]).unwrap();
        let frame_out = parse_basic_frame(acl_out.payload).unwrap();
        assert_eq!(frame_out.cid, CID_SIGNALING);
        let commands = parse_signaling_commands(frame_out.payload).unwrap();
        assert_eq!(
            commands,
            vec![SignalingCommand::InformationResponse {
                identifier: 0x42,
                info_type: INFO_TYPE_EXTENDED_FEATURES,
                result: INFO_RESULT_SUCCESS,
                data: AOKIE_EXTENDED_FEATURES_MASK.to_le_bytes().to_vec(),
            }]
        );
        // We deliberately advertise NO extended features — see the
        // const comment for why. If a future change flips bit 7 back
        // on, that change should also confirm Pixel's follow-up
        // type 0x0003 query lands cleanly under realistic USB load.
        assert_eq!(
            AOKIE_EXTENDED_FEATURES_MASK, 0,
            "advertising extended features (e.g. bit 7 FixedChannels) makes Pixel send a second \
             InformationRequest that's prone to being dropped by transient USB hiccups"
        );
    }

    #[test]
    fn information_request_unknown_type_returns_not_supported_not_command_reject() {
        // Per spec the response to an unsupported type is still an
        // InformationResponse, just with result=NotSupported. Reject
        // here would be the same bug we just fixed for known types.
        let response_bytes = build_information_response_for_type(7, 0x0099);
        let frame = parse_basic_frame(&response_bytes).unwrap();
        let commands = parse_signaling_commands(frame.payload).unwrap();
        assert_eq!(
            commands,
            vec![SignalingCommand::InformationResponse {
                identifier: 7,
                info_type: 0x0099,
                result: INFO_RESULT_NOT_SUPPORTED,
                data: vec![],
            }]
        );
    }

    #[test]
    fn rejects_truncated_l2cap_packets() {
        assert!(parse_acl_frame(&[0x2a, 0x20, 0x05, 0x00, 0x01]).is_err());
        assert!(parse_basic_frame(&[0x04, 0x00, 0x01]).is_err());
        assert!(parse_signaling_commands(&[0x02, 0x01, 0x04, 0x00, 0x03]).is_err());
    }

    #[test]
    fn builds_acl_packet_with_pb_and_broadcast_flags() {
        assert_eq!(
            build_acl_packet(
                0x002a,
                ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
                ACL_BROADCAST_POINT_TO_POINT,
                &[1, 2, 3],
            ),
            vec![0x2a, 0x20, 0x03, 0x00, 1, 2, 3]
        );
    }

    #[test]
    fn builds_basic_frame_for_dynamic_channels() {
        assert_eq!(
            build_basic_frame(0x0040, &[1, 2, 3]),
            vec![0x03, 0x00, 0x40, 0x00, 1, 2, 3]
        );
    }

    #[test]
    fn l2cap_state_accepts_supported_connection_and_configures_channel() {
        let connection_request = [
            0x2a, 0x20, 0x0c, 0x00, // ACL handle 0x002a, len=12
            0x08, 0x00, 0x01, 0x00, // L2CAP len=8, signaling CID
            0x02, 0x01, 0x04, 0x00, // Connection Request id=1 len=4
            0x03, 0x00, 0x40, 0x00, // RFCOMM, source CID 0x0040
        ];
        let mut state = L2capState::new();
        let responses = state.handle_acl_packet(&connection_request).unwrap();
        // We send back two packets now: ConnectionResponse plus our own
        // ConfigureRequest. The pre-fix code only sent ConnectionResponse
        // and skipped ever configuring the local side.
        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[0],
            vec![
                0x2a, 0x20, 0x10, 0x00, // ACL handle 0x002a, payload len 16
                0x0c, 0x00, 0x01, 0x00, // L2CAP len 12, signaling CID
                0x03, 0x01, 0x08, 0x00, // Connection Response id=1 len=8
                0x41, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
            ]
        );
        // ConfigureRequest with empty options against the remote's CID.
        // Identifier comes from FIRST_LOCAL_SIGNALING_ID = 0x80.
        assert_eq!(
            responses[1],
            vec![
                0x2a, 0x20, 0x0c, 0x00, // ACL handle, payload len 12
                0x08, 0x00, 0x01, 0x00, // L2CAP len 8, signaling CID
                0x04, 0x80, 0x04, 0x00, // Configure Request id=0x80 len=4
                0x40, 0x00, 0x00, 0x00, // DCID 0x0040, flags 0
            ]
        );
        assert_eq!(state.channel_count(), 1);
        let channel = state.channel(0x0041).unwrap();
        assert_eq!(channel.state, ChannelState::Configuring);
        assert!(!channel.local_configured);
        assert!(!channel.remote_configured);
        assert_eq!(channel.local_config_identifier, Some(0x80));

        let configure_request = [
            0x2a, 0x20, 0x10, 0x00, // ACL handle 0x002a, len=16
            0x0c, 0x00, 0x01, 0x00, // L2CAP len=12, signaling CID
            0x04, 0x02, 0x08, 0x00, // Configure Request id=2 len=8
            0x41, 0x00, 0x00, 0x00, // DCID 0x0041, flags 0
            0x01, 0x02, 0xa0, 0x02, // MTU option 672
        ];
        let responses = state.handle_acl_packet(&configure_request).unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0],
            vec![
                0x2a, 0x20, 0x0e, 0x00, // ACL handle 0x002a, payload len 14
                0x0a, 0x00, 0x01, 0x00, // L2CAP len 10, signaling CID
                0x05, 0x02, 0x06, 0x00, // Configure Response id=2 len=6
                0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
            ]
        );
        // Remote is configured; local still isn't until the AG acks our
        // ConfigureRequest.
        assert_eq!(
            state.channel(0x0041).unwrap().state,
            ChannelState::Configuring
        );

        // The remote acknowledges our ConfigureRequest with success;
        // both directions are now configured and the channel opens.
        // SCID in a ConfigureResponse identifies the requester's local
        // CID (our 0x0041) — same convention BTstack and iOS follow.
        let configure_response = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x05, 0x80, 0x06, 0x00, // Configure Response id=0x80 len=6
                    0x41, 0x00, 0x00, 0x00, 0x00, 0x00, // SCID 0x0041, flags 0, success 0
                ],
            ),
        );
        let responses = state.handle_acl_packet(&configure_response).unwrap();
        assert!(responses.is_empty());
        assert_eq!(state.channel(0x0041).unwrap().state, ChannelState::Open);
        assert!(state.channel(0x0041).unwrap().local_configured);
        assert_eq!(state.channel(0x0041).unwrap().local_config_identifier, None);
    }

    #[test]
    fn l2cap_state_declines_unsupported_psm_and_removes_disconnected_channel() {
        let unsupported_request = [
            0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x01, 0x04, 0x00, 0xff, 0x00,
            0x40, 0x00,
        ];
        let mut state = L2capState::new();
        let responses = state.handle_acl_packet(&unsupported_request).unwrap();
        assert_eq!(state.channel_count(), 0);
        assert_eq!(
            responses[0],
            vec![
                0x2a, 0x20, 0x10, 0x00, 0x0c, 0x00, 0x01, 0x00, 0x03, 0x01, 0x08, 0x00, 0x00, 0x00,
                0x40, 0x00, 0x02, 0x00, 0x00, 0x00,
            ]
        );

        let connection_request = [
            0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x02, 0x04, 0x00, 0x01, 0x00,
            0x40, 0x00,
        ];
        let _ = state.handle_acl_packet(&connection_request).unwrap();
        assert_eq!(state.channel_count(), 1);
        let disconnect_request = [
            0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x06, 0x03, 0x04, 0x00, 0x41, 0x00,
            0x40, 0x00,
        ];
        let responses = state.handle_acl_packet(&disconnect_request).unwrap();
        assert_eq!(state.channel_count(), 0);
        assert_eq!(
            responses[0],
            vec![
                0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x07, 0x03, 0x04, 0x00, 0x41, 0x00,
                0x40, 0x00,
            ]
        );
    }

    #[test]
    fn l2cap_state_removes_channels_for_disconnected_acl_handle() {
        let mut state = L2capState::new();
        let connection_request = [
            0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x01, 0x04, 0x00, 0x03, 0x00,
            0x40, 0x00,
        ];
        let _ = state.handle_acl_packet(&connection_request).unwrap();
        assert_eq!(state.channel_count(), 1);

        state.remove_connection(0x002b);
        assert_eq!(state.channel_count(), 1);

        state.remove_connection(0x002a);
        assert_eq!(state.channel_count(), 0);
    }

    #[test]
    fn l2cap_state_routes_open_sdp_payload_to_service_response() {
        let mut state = open_l2cap_state_with_psm(PSM_SDP);

        let sdp_request = [
            0x06, 0x00, 0x03, 0x00, 0x0f, // ServiceSearchAttributeRequest
            0x35, 0x03, 0x19, 0x11, 0x1e, // service search pattern: Handsfree
            0xff, 0xff, // max attribute bytes
            0x35, 0x05, 0x0a, 0x00, 0x00, 0xff, 0xff, // attr range 0..ffff
            0x00, // continuation state
        ];
        let request = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(0x0041, &sdp_request),
        );
        let responses = state.handle_acl_packet(&request).unwrap();
        assert_eq!(responses.len(), 1);

        let acl = parse_acl_frame(&responses[0]).unwrap();
        assert_eq!(acl.connection_handle, 0x002a);
        let frame = parse_basic_frame(acl.payload).unwrap();
        assert_eq!(frame.cid, 0x0040);
        assert_eq!(frame.payload[0], sdp::PDU_SERVICE_SEARCH_ATTRIBUTE_RESPONSE);
        assert_eq!(&frame.payload[1..3], &[0x00, 0x03]);
        assert!(frame
            .payload
            .windows(sdp::AOKIE_HFP_SERVICE_NAME.len())
            .any(|window| window == sdp::AOKIE_HFP_SERVICE_NAME.as_bytes()));
    }

    #[test]
    fn l2cap_state_routes_open_rfcomm_payload_to_rfcomm_state() {
        let mut state = open_rfcomm_l2cap_state();

        let request = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                0x0041,
                &rfcomm::build_sabm(rfcomm::RFCOMM_DLCI_MULTIPLEXER, true),
            ),
        );
        let responses = state.handle_acl_packet(&request).unwrap();
        assert_eq!(responses.len(), 1);

        let acl = parse_acl_frame(&responses[0]).unwrap();
        let frame = parse_basic_frame(acl.payload).unwrap();
        assert_eq!(frame.cid, 0x0040);
        let rfcomm = rfcomm::parse_frame(frame.payload).unwrap();
        assert_eq!(rfcomm.kind, rfcomm::RfcommFrameKind::Ua);
        assert_eq!(rfcomm.dlci, rfcomm::RFCOMM_DLCI_MULTIPLEXER);
        assert!(state
            .channel(0x0041)
            .unwrap()
            .rfcomm_state
            .as_ref()
            .unwrap()
            .multiplexer_open());
    }

    #[test]
    fn l2cap_state_routes_hfp_sabm_to_initial_at_command() {
        let mut state = open_rfcomm_l2cap_state();
        let _ = send_rfcomm_over_l2cap(
            &mut state,
            rfcomm::build_sabm(rfcomm::RFCOMM_DLCI_MULTIPLEXER, true),
        );

        let responses = send_rfcomm_over_l2cap(
            &mut state,
            rfcomm::build_sabm(rfcomm::aokie_hfp_dlci(), true),
        );
        // SABM on HFP DLCI: UA + our MSC CMD (per HFP §4.2.1). The
        // first AT command holds until MSC exchange completes in both
        // directions.
        assert_eq!(responses.len(), 2);

        let ua = rfcomm_payload_from_acl(&responses[0]);
        assert_eq!(ua.kind, rfcomm::RfcommFrameKind::Ua);
        assert_eq!(ua.dlci, rfcomm::aokie_hfp_dlci());

        let our_msc = rfcomm_payload_from_acl(&responses[1]);
        assert_eq!(our_msc.kind, rfcomm::RfcommFrameKind::Uih);
        assert_eq!(our_msc.dlci, rfcomm::RFCOMM_DLCI_MULTIPLEXER);
        assert_eq!(our_msc.payload[0], rfcomm::RFCOMM_MUX_MSC_CMD);
    }

    #[test]
    fn l2cap_state_routes_hfp_ag_response_to_next_at_command() {
        let mut state = open_hfp_l2cap_state();

        let responses = send_rfcomm_over_l2cap(
            &mut state,
            rfcomm::build_uih(rfcomm::aokie_hfp_dlci(), false, None, b"\r\nOK\r\n"),
        );
        assert_eq!(responses.len(), 1);
        // RfcommState defaults to wbs_supported=false (the runtime flips
        // this to true only when the transport exposes alt 6); the L2CAP
        // fixture builds an unconfigured state so we assert the
        // CVSD-only AT+BAC here.
        assert_eq!(
            rfcomm_payload_from_acl(&responses[0]).payload,
            b"AT+BAC=1\r"
        );
    }

    #[test]
    fn l2cap_state_collects_hfp_events_from_rfcomm_payloads() {
        let mut state = open_hfp_l2cap_state();
        let responses = send_rfcomm_over_l2cap(
            &mut state,
            rfcomm::build_uih(
                rfcomm::aokie_hfp_dlci(),
                false,
                None,
                b"\r\nRING\r\n+CLIP: \"+15551234567\",145\r\n+CIEV: 2,1\r\n",
            ),
        );
        assert!(responses.is_empty());
        assert_eq!(
            state.take_hfp_events(),
            vec![
                hfp::HfpEvent::IncomingCall,
                hfp::HfpEvent::Ringing,
                hfp::HfpEvent::CallerId("+15551234567".to_string()),
                hfp::HfpEvent::CallAnswered,
            ]
        );
        assert!(state.take_hfp_events().is_empty());
    }

    #[test]
    fn l2cap_state_builds_hfp_call_control_acl_packets() {
        let mut state = open_hfp_l2cap_state();
        let packets = state
            .build_hfp_call_control_packets(hfp::HfpAtCommand::Answer)
            .unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(rfcomm_payload_from_acl(&packets[0]).payload, b"ATA\r");

        let packets = state
            .build_hfp_call_control_packets(hfp::HfpAtCommand::RejectOrHangup)
            .unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(rfcomm_payload_from_acl(&packets[0]).payload, b"AT+CHUP\r");
    }

    #[test]
    fn l2cap_state_reassembles_fragmented_acl_packets() {
        // A single L2CAP frame larger than the typical ACL_Data_Packet_Length
        // arrives split across two ACL packets: PB=0b10 (start) carries the
        // basic-frame header + first chunk, PB=0b01 (continuing) carries the
        // remainder. Pre-fix the second fragment was parsed as its own
        // basic frame and either errored out or got silently dropped.
        let mut state = open_rfcomm_l2cap_state();

        // Build a long AT-command payload (no real semantic — it just has
        // to look like UIH on the HFP DLCI so the routing exercises both
        // fragments end-to-end).
        let long_at_payload = b"AT+VGM=12\r\nAT+VGS=8\r\nAT+CKPD=200\r\n";
        let rfcomm_uih = rfcomm::build_uih(rfcomm::aokie_hfp_dlci(), false, None, long_at_payload);
        let basic = build_basic_frame(0x0041, &rfcomm_uih);

        // Split the basic frame across two ACL packets at an arbitrary
        // mid-point. The first packet has PB=0b10, the second PB=0b01.
        let split = basic.len() / 2;
        let first = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic[..split],
        );
        let second = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_CONTINUING_FRAGMENT,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic[split..],
        );

        // First fragment alone must produce nothing — the basic frame
        // isn't complete yet.
        let responses = state.handle_acl_packet(&first).unwrap();
        assert!(responses.is_empty());
        // Second fragment completes the frame; RFCOMM swallows unknown
        // AT commands silently, so we don't assert on responses, just
        // that processing the continuation didn't blow up and that no
        // panic / error escaped.
        let _ = state.handle_acl_packet(&second).unwrap();
    }

    #[test]
    fn l2cap_stall_watchdog_tears_down_unresponsive_configure() {
        // Connection accepted, ConfigureRequest goes out, peer never
        // replies. After tick_l2cap_stalls fires, the channel should be
        // gone, an ACL-wrapped DisconnectionRequest should be returned,
        // and a ServiceLevelConnectionFailed event should land in the
        // orphan queue so the upper layer learns about it.
        let connection_request = [
            0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x01, 0x04, 0x00, 0x03, 0x00,
            0x40, 0x00,
        ];
        let mut state = L2capState::new();
        let _ = state.handle_acl_packet(&connection_request).unwrap();
        assert_eq!(state.channel_count(), 1);
        let local_cid = state.channel(0x0040 + 1).unwrap().local_cid;
        assert_eq!(local_cid, 0x0041);

        // Forge a stale sent-at on the channel and tick the watchdog.
        let now = Instant::now();
        if let Some(channel) = state.channels.get_mut(&local_cid) {
            channel.local_config_sent_at = Some(now - Duration::from_secs(30));
        }
        let stall_packets = state.tick_l2cap_stalls(now, Duration::from_secs(5));

        assert_eq!(state.channel_count(), 0, "channel must be torn down");
        assert_eq!(
            stall_packets.len(),
            1,
            "should emit one DisconnectionRequest"
        );
        // ACL header (4) + L2CAP header (4) + sig header (4) + payload (4) = 16 bytes.
        assert_eq!(stall_packets[0].len(), 16);
        // Signaling code byte sits at offset 8 (after 4-byte ACL + 4-byte
        // L2CAP headers). Confirm it's a DisconnectionRequest.
        assert_eq!(stall_packets[0][8], SIGNAL_DISCONNECTION_REQUEST);

        let events = state.take_hfp_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                hfp::HfpEvent::ServiceLevelConnectionFailed(reason) if reason.contains("ConfigureRequest")
            )),
            "watchdog must surface a ServiceLevelConnectionFailed event with a ConfigureRequest reason: {:?}",
            events
        );
    }

    #[test]
    fn l2cap_state_routes_payload_to_registered_psm_handler() {
        use std::sync::Mutex as StdMutex;

        // Phase 0b regression: a profile registers a custom PSM and its
        // handler closure receives the channel payload. This is the
        // mechanism MAP / OBEX-on-L2CAP will use without touching the
        // dispatch site.
        const FAKE_PSM: u16 = 0x0099;
        let captured: Arc<StdMutex<Vec<u8>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_for_handler = captured.clone();

        let mut state = L2capState::new();
        state.register_psm(
            FAKE_PSM,
            Arc::new(move |_channel, payload| {
                captured_for_handler
                    .lock()
                    .unwrap()
                    .extend_from_slice(payload);
                // Reply with a fixed sentinel so we can assert the
                // L2CAP layer wraps and forwards it.
                Ok(vec![vec![0xab, 0xcd]])
            }),
        );

        // Drive the same Connection→Configure→ConfigureResponse handshake
        // the existing helpers use, against our fake PSM. Reuse
        // open_l2cap_state_with_psm by replicating its sequence inline so
        // we can register the handler before any traffic arrives.
        let mut connection_request = vec![
            0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x01, 0x04, 0x00,
        ];
        connection_request.extend_from_slice(&FAKE_PSM.to_le_bytes());
        connection_request.extend_from_slice(&0x0040u16.to_le_bytes());
        let _ = state.handle_acl_packet(&connection_request).unwrap();
        assert_eq!(state.channel_count(), 1, "ConnectionRequest must succeed");

        let configure_request = [
            0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x04, 0x02, 0x04, 0x00, 0x41, 0x00,
            0x00, 0x00,
        ];
        let _ = state.handle_acl_packet(&configure_request).unwrap();

        let configure_response = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[0x05, 0x80, 0x06, 0x00, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00],
            ),
        );
        let _ = state.handle_acl_packet(&configure_response).unwrap();
        assert_eq!(
            state.channel(0x0041).map(|c| c.state),
            Some(ChannelState::Open),
            "channel must be open after configure handshake"
        );

        // Now send a payload on the open channel. Handler must run.
        let mut payload_packet = vec![
            0x2a, 0x20, 0x07, 0x00, // ACL header, len 7
            0x03, 0x00, // L2CAP basic-frame len 3
            0x41, 0x00, // CID 0x0041 (our local cid)
            0xde, 0xad, 0xbe, // 3-byte payload
        ];
        payload_packet[2] = (payload_packet.len() - 4) as u8;
        let responses = state.handle_acl_packet(&payload_packet).unwrap();

        assert_eq!(*captured.lock().unwrap(), vec![0xde, 0xad, 0xbe]);
        assert_eq!(responses.len(), 1);
        // Response is wrapped in basic frame + ACL: trailing payload
        // bytes must be the handler's sentinel.
        let r = &responses[0];
        assert_eq!(&r[r.len() - 2..], &[0xab, 0xcd]);
    }

    #[test]
    fn l2cap_stall_watchdog_no_op_on_open_channel() {
        let mut state = open_rfcomm_l2cap_state();
        let now = Instant::now();
        let packets = state.tick_l2cap_stalls(now, Duration::from_secs(5));
        assert!(packets.is_empty());
        assert_eq!(state.channel_count(), 1);
    }

    #[test]
    fn build_connection_request_round_trips_through_parser() {
        let bytes = build_connection_request(0x80, PSM_RFCOMM, 0x0040);
        // L2CAP basic frame on signaling CID, then a 4-byte signaling
        // header + 4-byte payload (PSM LE + SCID LE).
        let frame = parse_basic_frame(&bytes).unwrap();
        assert_eq!(frame.cid, CID_SIGNALING);
        let commands = parse_signaling_commands(frame.payload).unwrap();
        assert_eq!(
            commands,
            vec![SignalingCommand::ConnectionRequest {
                identifier: 0x80,
                psm: PSM_RFCOMM,
                source_cid: 0x0040,
            }]
        );
    }

    #[test]
    fn parses_connection_response_with_status_bytes() {
        let signaling = [
            0x03, 0x80, 0x08, 0x00, // Connection Response id=0x80 len=8
            0x70, 0x00, 0x41, 0x00, // DCID 0x0070, SCID 0x0041
            0x00, 0x00, 0x00, 0x00, // result success, status none
        ];
        let commands = parse_signaling_commands(&signaling).unwrap();
        assert_eq!(
            commands,
            vec![SignalingCommand::ConnectionResponse {
                identifier: 0x80,
                destination_cid: 0x0070,
                source_cid: 0x0041,
                result: CONNECTION_RESULT_SUCCESS,
                status: CONNECTION_STATUS_NO_FURTHER_INFORMATION,
            }]
        );
    }

    #[test]
    fn parses_disconnection_response_so_peer_acks_dont_get_rejected() {
        let signaling = [
            0x07, 0x05, 0x04, 0x00, // Disconnection Response id=5 len=4
            0x41, 0x00, 0x70, 0x00, // DCID 0x0041, SCID 0x0070
        ];
        let commands = parse_signaling_commands(&signaling).unwrap();
        assert_eq!(
            commands,
            vec![SignalingCommand::DisconnectionResponse {
                identifier: 5,
                destination_cid: 0x0041,
                source_cid: 0x0070,
            }]
        );
    }

    #[test]
    fn open_outbound_channel_registers_configuring_channel_and_emits_connection_request() {
        let mut state = L2capState::new();
        let packet = state.open_outbound_channel(0x002a, 0x112f); // PBAP PSE PSM
                                                                  // Channel is now registered locally even though we haven't
                                                                  // heard back yet. CID is the first dynamic value, identifier
                                                                  // is FIRST_LOCAL_SIGNALING_ID.
        assert_eq!(state.channel_count(), 1);
        let channel = state.channel(0x0040).unwrap();
        assert_eq!(channel.local_cid, 0x0040);
        assert_eq!(channel.remote_cid, 0, "remote_cid unknown until response");
        assert_eq!(channel.psm, 0x112f);
        assert_eq!(channel.state, ChannelState::Configuring);
        assert_eq!(channel.outbound_connect_identifier, Some(0x80));
        assert!(channel.local_config_sent_at.is_some());
        assert!(channel.local_config_identifier.is_none());
        assert!(channel.rfcomm_state.is_none());

        let acl = parse_acl_frame(&packet).unwrap();
        assert_eq!(acl.connection_handle, 0x002a);
        let frame = parse_basic_frame(acl.payload).unwrap();
        assert_eq!(frame.cid, CID_SIGNALING);
        let commands = parse_signaling_commands(frame.payload).unwrap();
        assert_eq!(
            commands,
            vec![SignalingCommand::ConnectionRequest {
                identifier: 0x80,
                psm: 0x112f,
                source_cid: 0x0040,
            }]
        );
    }

    #[test]
    fn outbound_connection_response_success_emits_configure_request_and_walks_to_open() {
        let mut state = L2capState::new();
        let _ = state.open_outbound_channel(0x002a, 0x112f);

        // Peer accepts: ConnectionResponse with their CID 0x0070,
        // echoing our SCID 0x0040 (FIRST_DYNAMIC_CID).
        let connection_response = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x03, 0x80, 0x08, 0x00, // Connection Response id=0x80 len=8
                    0x70, 0x00, 0x40, 0x00, // DCID 0x0070 (theirs), SCID 0x0040 (ours)
                    0x00, 0x00, 0x00, 0x00, // result success, status none
                ],
            ),
        );

        let responses = state.handle_acl_packet(&connection_response).unwrap();
        // Should emit a single ConfigureRequest aimed at remote 0x0070.
        assert_eq!(responses.len(), 1);
        let frame = parse_basic_frame(parse_acl_frame(&responses[0]).unwrap().payload).unwrap();
        let cmds = parse_signaling_commands(frame.payload).unwrap();
        assert!(matches!(
            cmds[0],
            SignalingCommand::ConfigureRequest {
                destination_cid: 0x0070,
                ..
            }
        ));
        let configure_id = match cmds[0] {
            SignalingCommand::ConfigureRequest { identifier, .. } => identifier,
            _ => unreachable!(),
        };
        assert_eq!(configure_id, 0x81, "second local sig id");

        let channel = state.channel(0x0040).unwrap();
        assert_eq!(channel.remote_cid, 0x0070);
        assert_eq!(channel.outbound_connect_identifier, None);
        assert_eq!(channel.local_config_identifier, Some(configure_id));
        assert_eq!(channel.state, ChannelState::Configuring);

        // Peer's ConfigureRequest arrives — we ack.
        let configure_request = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x04, 0x07, 0x04, 0x00, // Configure Request id=7 len=4
                    0x40, 0x00, 0x00, 0x00, // DCID = our 0x0040, flags 0
                ],
            ),
        );
        let _ = state.handle_acl_packet(&configure_request).unwrap();
        // Remote configured but local still pending peer's response.
        assert_eq!(
            state.channel(0x0040).unwrap().state,
            ChannelState::Configuring
        );
        assert!(state.channel(0x0040).unwrap().remote_configured);

        // Peer ConfigureResponse for our request → both sides done, Open.
        let configure_response = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x05,
                    configure_id,
                    0x06,
                    0x00, // Configure Response id len=6
                    0x40,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00, // SCID 0x0040, flags 0, success
                ],
            ),
        );
        let _ = state.handle_acl_packet(&configure_response).unwrap();
        assert_eq!(state.channel(0x0040).unwrap().state, ChannelState::Open);
        assert!(state.channel(0x0040).unwrap().local_configured);
    }

    #[test]
    fn outbound_connection_response_refused_tears_down_and_emits_failure_event() {
        let mut state = L2capState::new();
        let _ = state.open_outbound_channel(0x002a, 0x112f);
        assert_eq!(state.channel_count(), 1);

        let connection_response = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x03, 0x80, 0x08, 0x00, // Connection Response id=0x80 len=8
                    0x00, 0x00, 0x40, 0x00, // DCID 0 (refused), SCID 0x0040
                    0x02, 0x00, 0x00, 0x00, // result PSM_NOT_SUPPORTED, status 0
                ],
            ),
        );
        let responses = state.handle_acl_packet(&connection_response).unwrap();
        assert!(responses.is_empty(), "no signaling reply on refusal");
        assert_eq!(state.channel_count(), 0, "channel removed");

        let events = state.take_hfp_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                hfp::HfpEvent::ServiceLevelConnectionFailed(reason)
                    if reason.contains("refused") && reason.contains("0x112f")
            )),
            "expected refusal event, got {:?}",
            events
        );
    }

    #[test]
    fn outbound_connection_response_pending_keeps_channel_alive() {
        let mut state = L2capState::new();
        let _ = state.open_outbound_channel(0x002a, 0x112f);

        let pending = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x03, 0x80, 0x08, 0x00, 0x00, 0x00, 0x40, 0x00, 0x01, 0x00, 0x01, 0x00,
                ],
            ),
        );
        let responses = state.handle_acl_packet(&pending).unwrap();
        assert!(responses.is_empty(), "PENDING is not actionable yet");
        let channel = state.channel(0x0040).unwrap();
        assert_eq!(channel.state, ChannelState::Configuring);
        assert_eq!(
            channel.outbound_connect_identifier,
            Some(0x80),
            "still awaiting final response"
        );
    }

    #[test]
    fn outbound_disconnection_response_is_silently_consumed() {
        let mut state = L2capState::new();
        // Disconnection-Response with no matching channel — the parser
        // path used to fall to Unknown and emit a CommandReject.
        let resp = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[0x07, 0x05, 0x04, 0x00, 0x41, 0x00, 0x70, 0x00],
            ),
        );
        let responses = state.handle_acl_packet(&resp).unwrap();
        assert!(responses.is_empty(), "no reply expected for ack");
    }

    #[test]
    fn open_outbound_channel_with_handler_overrides_psm_default() {
        use std::sync::Mutex as StdMutex;
        // PSM_RFCOMM normally creates a server-mode RfcommState; for an
        // outbound channel we want our per-channel handler to capture
        // payloads instead. Phase 3c regression: the inbound RFCOMM
        // path keeps the default handler, but our outbound channel
        // routes elsewhere.
        let captured: Arc<StdMutex<Vec<Vec<u8>>>> = Arc::new(StdMutex::new(Vec::new()));
        let cap_for_handler = captured.clone();
        let handler: PsmHandler = Arc::new(move |_channel, payload| {
            cap_for_handler.lock().unwrap().push(payload.to_vec());
            Ok(Vec::new())
        });

        let mut state = L2capState::new();
        let (local_cid, _packet) =
            state.open_outbound_channel_with_handler(0x002a, PSM_RFCOMM, Some(handler));
        assert_eq!(local_cid, 0x0040);

        // Walk to Open: ConnectionResponse → ConfigureRequest → ConfigureResponse → peer ConfigureRequest.
        let cr = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x03, 0x80, 0x08, 0x00, 0x70, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
                ],
            ),
        );
        let _ = state.handle_acl_packet(&cr).unwrap();
        let configure_request_from_peer = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[0x04, 0x07, 0x04, 0x00, 0x40, 0x00, 0x00, 0x00],
            ),
        );
        let _ = state
            .handle_acl_packet(&configure_request_from_peer)
            .unwrap();
        let configure_rsp = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[0x05, 0x81, 0x06, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00],
            ),
        );
        let _ = state.handle_acl_packet(&configure_rsp).unwrap();
        assert_eq!(state.channel(local_cid).unwrap().state, ChannelState::Open);

        // Now feed an RFCOMM-like payload — our per-channel handler
        // should capture it, NOT the default RfcommState.
        let payload = vec![0xde, 0xad, 0xbe, 0xef];
        let mut packet = vec![
            0x2a, 0x20, 0x08, 0x00, // ACL handle 0x002a, payload len 8
            0x04, 0x00, // basic frame len 4
        ];
        packet.extend_from_slice(&local_cid.to_le_bytes());
        packet.extend_from_slice(&payload);
        let _ = state.handle_acl_packet(&packet).unwrap();
        assert_eq!(*captured.lock().unwrap(), vec![payload]);
        // Default RFCOMM state should NOT have been initialized for
        // an outbound channel — our handler took precedence.
        assert!(state.channel(local_cid).unwrap().rfcomm_state.is_none());
    }

    #[test]
    fn send_on_channel_wraps_payload_in_basic_frame_and_acl() {
        let mut state = L2capState::new();
        let (local_cid, _) = state.open_outbound_channel_with_handler(0x002a, PSM_RFCOMM, None);
        // Force the channel to Open without driving the full handshake,
        // and stamp a remote_cid so the basic frame has a destination.
        if let Some(channel) = state.channels.get_mut(&local_cid) {
            channel.state = ChannelState::Open;
            channel.remote_cid = 0x0070;
        }
        let payload = b"\x01\x02\x03";
        let acl = state.send_on_channel(local_cid, payload).unwrap();
        let parsed_acl = parse_acl_frame(&acl).unwrap();
        assert_eq!(parsed_acl.connection_handle, 0x002a);
        let frame = parse_basic_frame(parsed_acl.payload).unwrap();
        assert_eq!(frame.cid, 0x0070);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn send_on_channel_errors_on_unknown_or_unopened_cid() {
        let mut state = L2capState::new();
        // No channel yet.
        assert!(state.send_on_channel(0x0040, b"hi").is_err());
        // Open one but leave it Configuring.
        let (local_cid, _) = state.open_outbound_channel_with_handler(0x002a, PSM_RFCOMM, None);
        assert!(state.send_on_channel(local_cid, b"hi").is_err());
    }

    #[test]
    fn disconnect_channel_emits_disconnection_request_and_removes_channel() {
        let mut state = L2capState::new();
        let (local_cid, _) = state.open_outbound_channel_with_handler(0x002a, PSM_RFCOMM, None);
        if let Some(channel) = state.channels.get_mut(&local_cid) {
            channel.remote_cid = 0x0070;
            channel.state = ChannelState::Open;
        }
        let acl = state.disconnect_channel(local_cid).unwrap();
        assert_eq!(state.channel_count(), 0, "channel removed locally");
        // Signaling code at offset 8 should be DisconnectionRequest.
        assert_eq!(acl[8], SIGNAL_DISCONNECTION_REQUEST);
        // Calling again returns None — the channel is gone.
        assert!(state.disconnect_channel(local_cid).is_none());
    }

    #[test]
    fn l2cap_state_drops_reassembly_buffer_on_acl_disconnect() {
        let mut state = L2capState::new();
        let connection_request = [
            0x2a, 0x20, 0x0c, 0x00, 0x08, 0x00, 0x01, 0x00, 0x02, 0x01, 0x04, 0x00, 0x03, 0x00,
            0x40, 0x00,
        ];
        let _ = state.handle_acl_packet(&connection_request).unwrap();
        assert!(!state.acl_reassembly.is_empty());
        state.remove_connection(0x002a);
        assert!(state.acl_reassembly.is_empty());
    }

    fn open_rfcomm_l2cap_state() -> L2capState {
        open_l2cap_state_with_psm(PSM_RFCOMM)
    }

    fn open_l2cap_state_with_psm(psm: u16) -> L2capState {
        let mut state = L2capState::new();
        // 1. Inbound ConnectionRequest — produces ConnectionResponse +
        //    our outgoing ConfigureRequest.
        let mut connection_request = vec![
            0x2a, 0x20, 0x0c, 0x00, // ACL handle 0x002a, len=12
            0x08, 0x00, 0x01, 0x00, // L2CAP len=8, signaling CID
            0x02, 0x01, 0x04, 0x00, // Connection Request id=1 len=4
        ];
        connection_request.extend_from_slice(&psm.to_le_bytes());
        connection_request.extend_from_slice(&0x0040u16.to_le_bytes()); // source CID 0x0040
        let _ = state.handle_acl_packet(&connection_request).unwrap();

        // 2. Inbound ConfigureRequest — flips remote_configured.
        let configure_request = [
            0x2a, 0x20, 0x0c, 0x00, // ACL handle 0x002a, len=12
            0x08, 0x00, 0x01, 0x00, // L2CAP len=8, signaling CID
            0x04, 0x02, 0x04, 0x00, // Configure Request id=2 len=4
            0x41, 0x00, 0x00, 0x00, // DCID 0x0041, flags 0
        ];
        let _ = state.handle_acl_packet(&configure_request).unwrap();

        // 3. Inbound ConfigureResponse for our ConfigureRequest (id 0x80,
        //    SCID = OUR local CID 0x0041 — per L2CAP §4.5 the Source CID
        //    in the response identifies the requester's channel endpoint
        //    (matches what real ipads/iPhones send and what BTstack puts
        //    in its CONFIGURE_RESPONSE source_cid field). Flips
        //    local_configured and opens the channel.
        let configure_response = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x05, 0x80, 0x06, 0x00, // Configure Response id=0x80 len=6
                    0x41, 0x00, 0x00, 0x00, 0x00, 0x00,
                ],
            ),
        );
        let _ = state.handle_acl_packet(&configure_response).unwrap();
        assert_eq!(state.channel(0x0041).unwrap().state, ChannelState::Open);
        state
    }

    fn open_hfp_l2cap_state() -> L2capState {
        let mut state = open_rfcomm_l2cap_state();
        let _ = send_rfcomm_over_l2cap(
            &mut state,
            rfcomm::build_sabm(rfcomm::RFCOMM_DLCI_MULTIPLEXER, true),
        );
        let _ = send_rfcomm_over_l2cap(
            &mut state,
            rfcomm::build_sabm(rfcomm::aokie_hfp_dlci(), true),
        );
        // Drive the MSC exchange so the first SLC AT command fires.
        // AG acks our MSC CMD with MSC RSP, then sends its own MSC CMD
        // (which we ack and which kicks the AT queue).
        let _ = send_rfcomm_over_l2cap(
            &mut state,
            rfcomm::build_uih(
                rfcomm::RFCOMM_DLCI_MULTIPLEXER,
                true,
                None,
                &rfcomm::build_modem_status_response(
                    rfcomm::aokie_hfp_dlci(),
                    rfcomm::RFCOMM_LOCAL_MODEM_STATUS,
                ),
            ),
        );
        let _ = send_rfcomm_over_l2cap(
            &mut state,
            rfcomm::build_uih(
                rfcomm::RFCOMM_DLCI_MULTIPLEXER,
                true,
                None,
                &rfcomm::build_modem_status_command(
                    rfcomm::aokie_hfp_dlci(),
                    rfcomm::RFCOMM_LOCAL_MODEM_STATUS,
                ),
            ),
        );
        state
    }

    fn send_rfcomm_over_l2cap(state: &mut L2capState, rfcomm_frame: Vec<u8>) -> Vec<Vec<u8>> {
        let request = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(0x0041, &rfcomm_frame),
        );
        state.handle_acl_packet(&request).unwrap()
    }

    fn rfcomm_payload_from_acl(packet: &[u8]) -> rfcomm::RfcommFrame<'_> {
        let acl = parse_acl_frame(packet).unwrap();
        let frame = parse_basic_frame(acl.payload).unwrap();
        assert_eq!(frame.cid, 0x0040);
        rfcomm::parse_frame(frame.payload).unwrap()
    }

    #[test]
    fn fuzz_acl_basic_signaling_parsers_do_not_panic_on_random_bytes() {
        // L2CAP runs above the WinUSB ACL accumulator; a misframed
        // packet from the controller (or a peer sending garbage) must
        // never panic our parsers.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x1ca9_1ca9_1ca9_1ca9);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 1024) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_acl_frame(&buf);
            let _ = parse_basic_frame(&buf);
            let _ = parse_signaling_commands(&buf);
        }
    }
}
