//! Outbound HFP connect runtime — the orchestration half of HARD-001's
//! reconnect (`phone.connect`). After the runtime pages a bonded phone
//! and the ACL comes up, this state machine does what Bluedroid won't
//! do for a paged AG: discover the Hands-Free AG RFCOMM channel via
//! SDP, open an outbound L2CAP/RFCOMM channel, and install + kick the
//! initiator-side `HfpClientState` (see `hfp_client.rs`), whose SLC
//! events then flow through the exact same `take_hfp_events` surface
//! as an inbound connection.
//!
//! Shape mirrors `PbapRuntime` (tick-based, per-channel byte-capture
//! handler, watchdog), but ends at the SLC kickoff — once the HFP
//! client is installed on the channel, the per-channel upper handler
//! routes every subsequent RFCOMM frame straight into it and this
//! runtime reports `Done`.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use super::l2cap::{ChannelState, L2capState, PsmHandler, PSM_RFCOMM, PSM_SDP};
use super::sdp_client::{
    build_service_search_attribute_request, extract_rfcomm_channel, iter_records,
    parse_service_search_attribute_response, UUID_HANDSFREE_AG,
};

/// SDP + RFCOMM-open must progress within this window or we fail the
/// connect (the caller then tears the half-open ACL down so the UI
/// never shows a dead "connected" link — the exact HARD-001 symptom).
const HFP_CONNECT_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfpConnectEvent {
    /// SLC kickoff sent — the HFP client now owns the channel. The SLC
    /// outcome itself surfaces via `L2capState::take_hfp_events`
    /// (ServiceLevelConnectionReady / ...Failed).
    SlcKicked,
    /// Discovery / channel setup failed. Caller should disconnect the
    /// ACL: a paged link with no SLC is a dead half-link.
    Failed(String),
}

#[derive(Debug)]
enum Phase {
    Idle,
    AwaitingSdpOpen { local_cid: u16 },
    AwaitingSdpResponse { local_cid: u16, buffer: Vec<u8> },
    AwaitingRfcommOpen { server_channel: u8, local_cid: u16 },
    Done,
    Failed(String),
}

pub struct HfpConnectRuntime {
    connection_handle: u16,
    wbs_supported: bool,
    /// Phase 4: advertise call waiting / 3-way on the outbound SLC.
    call_waiting_enabled: bool,
    phase: Phase,
    pending_events: Vec<HfpConnectEvent>,
    /// Inbound bytes captured by the SDP channel's upper handler,
    /// keyed by local_cid (same pattern as PbapRuntime).
    inbound_buffer: Arc<StdMutex<Vec<(u16, Vec<u8>)>>>,
    sdp_transaction_id: u16,
    last_progress_at: Instant,
}

impl HfpConnectRuntime {
    pub fn new(connection_handle: u16, wbs_supported: bool, call_waiting_enabled: bool) -> Self {
        Self {
            connection_handle,
            wbs_supported,
            call_waiting_enabled,
            phase: Phase::Idle,
            pending_events: Vec::new(),
            inbound_buffer: Arc::new(StdMutex::new(Vec::new())),
            sdp_transaction_id: 0x0001,
            last_progress_at: Instant::now(),
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.phase, Phase::Failed(_))
    }

    /// The failure reason once `is_failed()` — for logs/diagnostics.
    pub fn failure_reason(&self) -> Option<&str> {
        match &self.phase {
            Phase::Failed(reason) => Some(reason),
            _ => None,
        }
    }

    pub fn take_events(&mut self) -> Vec<HfpConnectEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Open the outbound SDP channel. Call once, right after the ACL
    /// ConnectionComplete for our page. Returns ACL packets to write.
    pub fn start(&mut self, l2cap_state: &mut L2capState) -> Result<Vec<Vec<u8>>, String> {
        if !matches!(self.phase, Phase::Idle) {
            return Err(format!(
                "HFP connect start called in non-Idle phase: {:?}",
                self.phase
            ));
        }
        let (local_cid, packet) = l2cap_state.open_outbound_channel_with_handler(
            self.connection_handle,
            PSM_SDP,
            Some(self.make_buffer_handler()),
        );
        self.phase = Phase::AwaitingSdpOpen { local_cid };
        self.last_progress_at = Instant::now();
        Ok(vec![packet])
    }

    /// Drive the state machine. Same contract as `PbapRuntime::tick`.
    pub fn tick(&mut self, l2cap_state: &mut L2capState) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        let inbound = std::mem::take(
            &mut *self
                .inbound_buffer
                .lock()
                .map_err(|_| "HFP connect inbound buffer poisoned".to_string())?,
        );
        let had_inbound = !inbound.is_empty();
        for (cid, bytes) in inbound {
            self.consume_inbound(cid, bytes, l2cap_state, &mut out)?;
        }
        if had_inbound {
            self.last_progress_at = Instant::now();
        }
        self.drive_outbound(l2cap_state, &mut out)?;
        let in_progress = matches!(
            self.phase,
            Phase::AwaitingSdpOpen { .. }
                | Phase::AwaitingSdpResponse { .. }
                | Phase::AwaitingRfcommOpen { .. }
        );
        if in_progress && self.last_progress_at.elapsed() >= HFP_CONNECT_INACTIVITY_TIMEOUT {
            let active_cid = match &self.phase {
                Phase::AwaitingSdpOpen { local_cid }
                | Phase::AwaitingSdpResponse { local_cid, .. }
                | Phase::AwaitingRfcommOpen { local_cid, .. } => Some(*local_cid),
                _ => None,
            };
            self.fail(
                "HFP connect stalled: the phone stopped answering during SDP/RFCOMM setup"
                    .to_string(),
                active_cid,
                l2cap_state,
                &mut out,
            );
        }
        Ok(out)
    }

    fn make_buffer_handler(&self) -> PsmHandler {
        let buf = self.inbound_buffer.clone();
        Arc::new(move |channel, payload| {
            buf.lock()
                .map_err(|_| "HFP connect inbound buffer poisoned".to_string())?
                .push((channel.local_cid, payload.to_vec()));
            Ok(Vec::new())
        })
    }

    /// Upper handler for the outbound RFCOMM channel: every frame goes
    /// straight into the channel's `HfpClientState` (installed by
    /// `start_hfp_client` at kickoff time). Frames arriving before the
    /// kickoff are dropped — the peer never sends RFCOMM before our
    /// SABM, so this only guards against garbage.
    fn make_hfp_routing_handler() -> PsmHandler {
        Arc::new(move |channel, payload| match channel.hfp_client.as_mut() {
            Some(client) => client.handle_packet(payload),
            None => {
                eprintln!(
                    "[AokieRadio] HFP connect: RFCOMM bytes on cid 0x{:04x} before kickoff — dropped",
                    channel.local_cid
                );
                Ok(Vec::new())
            }
        })
    }

    fn consume_inbound(
        &mut self,
        cid: u16,
        bytes: Vec<u8>,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), String> {
        let current = std::mem::replace(&mut self.phase, Phase::Done);
        match current {
            Phase::AwaitingSdpResponse {
                local_cid,
                mut buffer,
            } if local_cid == cid => {
                buffer.extend_from_slice(&bytes);
                match parse_service_search_attribute_response(&buffer) {
                    Ok(resp) if resp.continuation_state.is_empty() => {
                        let server_channel = match extract_ag_channel(resp.attribute_lists) {
                            Ok(c) => c,
                            Err(e) => {
                                self.fail(
                                    format!("HFP connect SDP: {}", e),
                                    Some(local_cid),
                                    l2cap_state,
                                    out,
                                );
                                return Ok(());
                            }
                        };
                        if let Some(disc) = l2cap_state.disconnect_channel(local_cid) {
                            out.push(disc);
                        }
                        // We own the ONLY RFCOMM session on this ACL
                        // (we paged; the phone initiates nothing), so
                        // the own-mux client path is the correct one —
                        // Bluedroid's one-session-per-peer limit is
                        // about a SECOND session, which doesn't exist
                        // here.
                        let (rfcomm_cid, conn_req) = l2cap_state
                            .open_outbound_channel_with_handler(
                                self.connection_handle,
                                PSM_RFCOMM,
                                Some(Self::make_hfp_routing_handler()),
                            );
                        out.push(conn_req);
                        eprintln!(
                            "[AokieRadio] HFP connect SDP done — AG RFCOMM channel {} → opening cid 0x{:04x}",
                            server_channel, rfcomm_cid
                        );
                        self.phase = Phase::AwaitingRfcommOpen {
                            server_channel,
                            local_cid: rfcomm_cid,
                        };
                    }
                    Ok(_) => {
                        self.fail(
                            "HFP connect SDP: continuation state present (record too large?)"
                                .to_string(),
                            Some(local_cid),
                            l2cap_state,
                            out,
                        );
                    }
                    Err(_) => {
                        // Partial response — keep buffering.
                        self.phase = Phase::AwaitingSdpResponse { local_cid, buffer };
                    }
                }
            }
            other => {
                // Bytes for a phase that doesn't consume them (e.g. a
                // late SDP fragment after failure) — restore and drop.
                self.phase = other;
            }
        }
        Ok(())
    }

    fn drive_outbound(
        &mut self,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), String> {
        match &self.phase {
            Phase::AwaitingSdpOpen { local_cid } => {
                let cid = *local_cid;
                if let Some(channel) = l2cap_state.channel(cid) {
                    if channel.state == ChannelState::Open {
                        let req = build_service_search_attribute_request(
                            self.sdp_transaction_id,
                            UUID_HANDSFREE_AG,
                            0xffff,
                        );
                        let acl = l2cap_state.send_on_channel(cid, &req)?;
                        out.push(acl);
                        self.phase = Phase::AwaitingSdpResponse {
                            local_cid: cid,
                            buffer: Vec::new(),
                        };
                        self.last_progress_at = Instant::now();
                    }
                }
            }
            Phase::AwaitingRfcommOpen {
                server_channel,
                local_cid,
            } => {
                let cid = *local_cid;
                let channel_num = *server_channel;
                if let Some(channel) = l2cap_state.channel(cid) {
                    if channel.state == ChannelState::Open {
                        let sabm = l2cap_state.start_hfp_client(
                            cid,
                            channel_num,
                            self.wbs_supported,
                            self.call_waiting_enabled,
                        )?;
                        out.push(sabm);
                        eprintln!(
                            "[AokieRadio] HFP connect: RFCOMM cid 0x{:04x} open — SLC kickoff (AG channel {})",
                            cid, channel_num
                        );
                        self.pending_events.push(HfpConnectEvent::SlcKicked);
                        self.phase = Phase::Done;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn fail(
        &mut self,
        reason: String,
        active_cid: Option<u16>,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) {
        eprintln!("[AokieRadio] HFP connect FAILED: {}", reason);
        if let Some(cid) = active_cid {
            if let Some(disc) = l2cap_state.disconnect_channel(cid) {
                out.push(disc);
            }
        }
        self.pending_events
            .push(HfpConnectEvent::Failed(reason.clone()));
        self.phase = Phase::Failed(reason);
    }
}

/// Pull the AG's RFCOMM server channel out of the SDP attribute lists.
fn extract_ag_channel(attribute_lists: &[u8]) -> Result<u8, String> {
    for record in iter_records(attribute_lists)? {
        if let Some(channel) = extract_rfcomm_channel(record)? {
            return Ok(channel);
        }
    }
    Err("the phone's Hands-Free AG record did not advertise an RFCOMM channel".to_string())
}

#[cfg(test)]
mod tests {
    use super::super::l2cap::{
        build_acl_packet, build_basic_frame, ACL_BROADCAST_POINT_TO_POINT,
        ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE, CID_SIGNALING,
    };
    use super::*;

    const HANDLE: u16 = 0x002a;

    /// Minimal HFP-AG SDP response: one record whose
    /// ProtocolDescriptorList carries L2CAP + RFCOMM(channel). Byte
    /// encoding mirrors `pbap_runtime`'s test fixture.
    fn ag_sdp_response(transaction_id: u16, rfcomm_channel: u8) -> Vec<u8> {
        let l2cap_seq: Vec<u8> = vec![0x35, 0x03, 0x19, 0x01, 0x00];
        let rfcomm_seq: Vec<u8> = vec![0x35, 0x05, 0x19, 0x00, 0x03, 0x08, rfcomm_channel];
        let pdl_payload: Vec<u8> = [l2cap_seq, rfcomm_seq].concat();
        let mut pdl_seq: Vec<u8> = vec![0x35, pdl_payload.len() as u8];
        pdl_seq.extend_from_slice(&pdl_payload);
        let mut attr_pair: Vec<u8> = vec![0x09, 0x00, 0x04];
        attr_pair.extend_from_slice(&pdl_seq);
        let mut attr_list_seq: Vec<u8> = vec![0x35, attr_pair.len() as u8];
        attr_list_seq.extend_from_slice(&attr_pair);
        let mut outer_seq: Vec<u8> = vec![0x35, attr_list_seq.len() as u8];
        outer_seq.extend_from_slice(&attr_list_seq);
        let mut pdu: Vec<u8> = Vec::new();
        pdu.push(0x07);
        pdu.extend_from_slice(&transaction_id.to_be_bytes());
        let attr_byte_count = outer_seq.len() as u16;
        let parameter_length: u16 = 2 + attr_byte_count + 1;
        pdu.extend_from_slice(&parameter_length.to_be_bytes());
        pdu.extend_from_slice(&attr_byte_count.to_be_bytes());
        pdu.extend_from_slice(&outer_seq);
        pdu.push(0x00);
        pdu
    }

    /// Walk an outbound L2CAP channel from Configuring to Open by
    /// injecting the AG's ConnectionResponse + ConfigureRequest +
    /// ConfigureResponse. Copied from pbap_runtime's test harness —
    /// the signaling-identifier allocation runs 0x80,0x81,0x82 for the
    /// first (SDP) channel and 0x83,0x84 for the second (RFCOMM).
    fn walk_outbound_l2cap_to_open(
        l2cap: &mut L2capState,
        local_cid: u16,
        remote_cid: u16,
        outbound_connect_id: u8,
        configure_id: u8,
    ) {
        let cid_bytes = local_cid.to_le_bytes();
        let rcid_bytes = remote_cid.to_le_bytes();
        let conn_resp = build_acl_packet(
            HANDLE,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x03,
                    outbound_connect_id,
                    0x08,
                    0x00,
                    rcid_bytes[0],
                    rcid_bytes[1],
                    cid_bytes[0],
                    cid_bytes[1],
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                ],
            ),
        );
        let _ = l2cap.handle_acl_packet(&conn_resp).unwrap();
        let cfg_req = build_acl_packet(
            HANDLE,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x04,
                    0x07,
                    0x04,
                    0x00,
                    cid_bytes[0],
                    cid_bytes[1],
                    0x00,
                    0x00,
                ],
            ),
        );
        let _ = l2cap.handle_acl_packet(&cfg_req).unwrap();
        let cfg_resp = build_acl_packet(
            HANDLE,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x05,
                    configure_id,
                    0x06,
                    0x00,
                    cid_bytes[0],
                    cid_bytes[1],
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                ],
            ),
        );
        let _ = l2cap.handle_acl_packet(&cfg_resp).unwrap();
        assert_eq!(l2cap.channel(local_cid).unwrap().state, ChannelState::Open);
    }

    fn feed_channel_bytes(l2cap: &mut L2capState, cid: u16, payload: &[u8]) {
        let basic = build_basic_frame(cid, payload);
        let acl = build_acl_packet(
            HANDLE,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        );
        let _ = l2cap.handle_acl_packet(&acl).unwrap();
    }

    #[test]
    fn full_flow_reaches_slc_kickoff() {
        let mut l2cap = L2capState::new();
        let mut runtime = HfpConnectRuntime::new(HANDLE, false, false);
        let packets = runtime.start(&mut l2cap).expect("start");
        assert_eq!(packets.len(), 1, "SDP ConnectionRequest expected");
        assert!(runtime.start(&mut l2cap).is_err(), "double start refused");

        // SDP channel = first dynamic CID.
        let sdp_cid = 0x0040;
        walk_outbound_l2cap_to_open(&mut l2cap, sdp_cid, 0x0070, 0x80, 0x81);
        let out = runtime.tick(&mut l2cap).expect("tick sdp query");
        assert_eq!(out.len(), 1, "SDP query after channel open");

        feed_channel_bytes(&mut l2cap, sdp_cid, &ag_sdp_response(0x0001, 1));
        let out = runtime.tick(&mut l2cap).expect("tick rfcomm open");
        assert_eq!(out.len(), 2, "SDP disconnect + RFCOMM ConnectionRequest");

        // RFCOMM channel takes the next CID; identifiers continue 0x83/0x84.
        let rfcomm_cid = 0x0041;
        walk_outbound_l2cap_to_open(&mut l2cap, rfcomm_cid, 0x0071, 0x83, 0x84);
        let out = runtime.tick(&mut l2cap).expect("tick kickoff");
        assert_eq!(out.len(), 1, "multiplexer SABM after RFCOMM open");
        assert!(runtime.is_done());
        let events = runtime.take_events();
        assert!(events.contains(&HfpConnectEvent::SlcKicked), "{events:?}");
        assert!(
            l2cap.channel(rfcomm_cid).unwrap().hfp_client.is_some(),
            "HFP client must own the RFCOMM channel"
        );
    }

    #[test]
    fn missing_ag_record_fails_cleanly() {
        let mut l2cap = L2capState::new();
        let mut runtime = HfpConnectRuntime::new(HANDLE, false, false);
        runtime.start(&mut l2cap).expect("start");
        let sdp_cid = 0x0040;
        walk_outbound_l2cap_to_open(&mut l2cap, sdp_cid, 0x0070, 0x80, 0x81);
        runtime.tick(&mut l2cap).expect("tick sdp query");

        // One empty record — no ProtocolDescriptorList.
        let inner_record: Vec<u8> = vec![0x35, 0x00];
        let mut outer: Vec<u8> = vec![0x35, inner_record.len() as u8];
        outer.extend_from_slice(&inner_record);
        let mut pdu: Vec<u8> = vec![0x07, 0x00, 0x01];
        let attr_byte_count = outer.len() as u16;
        let parameter_length: u16 = 2 + attr_byte_count + 1;
        pdu.extend_from_slice(&parameter_length.to_be_bytes());
        pdu.extend_from_slice(&attr_byte_count.to_be_bytes());
        pdu.extend_from_slice(&outer);
        pdu.push(0x00);
        feed_channel_bytes(&mut l2cap, sdp_cid, &pdu);

        runtime.tick(&mut l2cap).expect("tick fail");
        assert!(runtime.is_failed());
        assert!(runtime
            .take_events()
            .iter()
            .any(|e| matches!(e, HfpConnectEvent::Failed(_))));
    }

    #[test]
    fn inactivity_watchdog_fails_a_silent_setup() {
        let mut l2cap = L2capState::new();
        let mut runtime = HfpConnectRuntime::new(HANDLE, false, false);
        runtime.start(&mut l2cap).expect("start");
        // No peer answer at all. Backdate progress past the timeout.
        runtime.last_progress_at =
            Instant::now() - HFP_CONNECT_INACTIVITY_TIMEOUT - Duration::from_secs(1);
        runtime.tick(&mut l2cap).expect("tick watchdog");
        assert!(runtime.is_failed());
    }
}
