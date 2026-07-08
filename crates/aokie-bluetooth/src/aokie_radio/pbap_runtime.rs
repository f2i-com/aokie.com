//! PBAP runtime — the state machine that orchestrates
//! SDP discovery → outbound L2CAP → RFCOMM client → OBEX/PBAP for a
//! phonebook fetch on a single AG.
//!
//! Phase 3d of the MAP/PBAP plan: this is the glue that drives
//! `sdp_client`, `RfcommClientState`, and `PbapPceSession` against an
//! `L2capState`. It exposes a tick-based API the runtime IO loop can
//! call after every inbound ACL packet. Phase 3e will wire it into
//! `runtime.rs`; for now this module is pure and unit-tested in
//! isolation against an in-process `L2capState`.
//!
//! Lifecycle:
//! 1. Caller constructs `PbapRuntime::new(connection_handle)` after
//!    the AG completes service-level connection.
//! 2. Caller calls `start(l2cap_state)` to open the SDP outbound
//!    channel. Returns the ACL ConnectionRequest packet.
//! 3. After every `l2cap_state.handle_acl_packet(...)` the caller
//!    calls `tick(l2cap_state)`, drains the produced ACL packets,
//!    and writes them via the transport.
//! 4. When events arrive, `take_events()` surfaces them.
//!    `ContactsFetched` means the phonebook is ready to persist.
//! 5. After `Done`/`Failed`, no further calls are valid; construct a
//!    fresh `PbapRuntime` for the next AG.

#![allow(dead_code)] // Phase 3d — runtime integration follows.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

/// If we're mid-fetch and no inbound RFCOMM bytes have arrived in this
/// long, give up and mark PBAP failed so MAP / etc. aren't blocked
/// forever waiting on us. Pixel/Bluedroid's PSE occasionally streams a
/// few hundred bytes of a multi-packet GET response and then goes
/// silent (no spec'd recovery on either side); the receptionist can run
/// number-only without the contacts batch, so failing fast is better
/// than holding the queue.
const PBAP_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(15);

use super::l2cap::{ChannelState, L2capState, PsmHandler, PSM_RFCOMM, PSM_SDP};
use super::obex::{FixedPayload, Packet};
use super::pbap::{PbapPceSession, PbapState};
use super::rfcomm::{ClientDlciEvent, RfcommClientEvent, RfcommClientState};
use super::sdp_client::{
    build_service_search_attribute_request, extract_rfcomm_channel, iter_records,
    parse_service_search_attribute_response, UUID_PBAP_PSE,
};
use super::vcard::Contact;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PbapRuntimeEvent {
    /// Phonebook fetch completed successfully. `contacts` is the full
    /// vCard set parsed from `pb.vcf`. The runtime is now in `Done`
    /// and any L2CAP/RFCOMM channels it opened have been politely
    /// torn down (ACL packets included in the same `tick`).
    ContactsFetched(Vec<Contact>),
    /// Anything went wrong — SDP didn't return a record, the AG
    /// refused our CONNECT, RFCOMM stalled, etc. The reason carries
    /// enough context to log; the runtime is now in `Failed` and
    /// remaining channels (if any) have been disconnect-requested.
    Failed(String),
}

/// Internal phase. Each variant captures the cid we're working on so
/// late inbound bytes from a previously-active channel can be
/// distinguished from current ones.
#[derive(Debug)]
enum PbapRuntimePhase {
    Idle,
    /// Outbound L2CAP to PSM_SDP in flight; we'll send the
    /// ServiceSearchAttributeRequest once the channel goes Open.
    AwaitingSdpOpen {
        local_cid: u16,
    },
    /// SDP request sent; reassembling the response in `buffer`.
    AwaitingSdpResponse {
        local_cid: u16,
        buffer: Vec<u8>,
    },
    /// SDP told us PBAP PSE lives on `server_channel`; outbound
    /// L2CAP to PSM_RFCOMM is in flight (legacy own-session path,
    /// kept for the unit-test harness which doesn't simulate the
    /// inbound HFP RFCOMM mux).
    AwaitingRfcommOpen {
        server_channel: u8,
        local_cid: u16,
    },
    /// RFCOMM L2CAP is Open; we kicked off the multiplexer SABM and
    /// are letting `RfcommClientState` drive the handshake.
    DrivingRfcomm {
        local_cid: u16,
        client: RfcommClientState,
    },
    /// RFCOMM channel up; running OBEX/PBAP against `session`.
    /// `obex_buffer` reassembles OBEX packets across RFCOMM UIH
    /// fragments — a single OBEX response can span multiple UIH
    /// frames when its declared length exceeds the negotiated
    /// max_frame_size.
    DrivingPbap {
        local_cid: u16,
        client: RfcommClientState,
        session: PbapPceSession,
        obex_buffer: Vec<u8>,
    },
    /// Shared-mux variant of AwaitingRfcommOpen — when the inbound
    /// HFP RFCOMM L2CAP channel is already open with the multiplexer
    /// up, we attach a new client DLCI to the existing session
    /// instead of opening a parallel PSM 0x0003 channel that
    /// Bluedroid would silently refuse (see project memory
    /// `bluedroid_one_rfcomm_session`).
    AwaitingSharedDlciOpen {
        local_cid: u16,
        target_dlci: u8,
    },
    /// Shared-mux variant of DrivingPbap. We don't own a
    /// `RfcommClientState` here — the L2CAP layer's
    /// `RfcommState::client_dlci_*` API delivers events via
    /// `take_rfcomm_client_events`.
    DrivingPbapShared {
        local_cid: u16,
        target_dlci: u8,
        session: PbapPceSession,
        obex_buffer: Vec<u8>,
    },
    Done,
    Failed(String),
}

pub struct PbapRuntime {
    connection_handle: u16,
    phase: PbapRuntimePhase,
    pending_events: Vec<PbapRuntimeEvent>,
    /// Inbound bytes captured by the L2CAP per-channel handler. Keyed
    /// by local_cid so we can tell SDP and RFCOMM payloads apart even
    /// if both channels somehow stay open. Drained on every `tick`.
    inbound_buffer: Arc<StdMutex<Vec<(u16, Vec<u8>)>>>,
    /// SDP transaction id for our query. Wraps but won't realistically
    /// — we only do one PBAP query per AG connect.
    sdp_transaction_id: u16,
    /// Last time we observed inbound progress (any RFCOMM bytes for the
    /// shared DLCI, or any phase-advancing tick on the legacy own-L2CAP
    /// path). Used by the inactivity watchdog to fail PBAP rather than
    /// blocking MAP forever when Pixel's PSE strands us mid-stream.
    last_progress_at: Instant,
}

impl PbapRuntime {
    pub fn new(connection_handle: u16) -> Self {
        Self {
            connection_handle,
            phase: PbapRuntimePhase::Idle,
            pending_events: Vec::new(),
            inbound_buffer: Arc::new(StdMutex::new(Vec::new())),
            sdp_transaction_id: 0x0001,
            last_progress_at: Instant::now(),
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, PbapRuntimePhase::Done)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.phase, PbapRuntimePhase::Failed(_))
    }

    pub fn take_events(&mut self) -> Vec<PbapRuntimeEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Open the SDP outbound channel. Returns the ACL ConnectionRequest
    /// packet for the runtime caller to write to the transport.
    pub fn start(&mut self, l2cap_state: &mut L2capState) -> Result<Vec<Vec<u8>>, String> {
        if !matches!(self.phase, PbapRuntimePhase::Idle) {
            return Err(format!(
                "PBAP start called in non-Idle phase: {:?}",
                self.phase
            ));
        }
        let (local_cid, packet) = l2cap_state.open_outbound_channel_with_handler(
            self.connection_handle,
            PSM_SDP,
            Some(self.make_handler()),
        );
        self.phase = PbapRuntimePhase::AwaitingSdpOpen { local_cid };
        self.last_progress_at = Instant::now();
        Ok(vec![packet])
    }

    /// Drive the state machine forward. Drain captured inbound bytes,
    /// inspect L2CAP channel transitions, advance phases, and produce
    /// outbound ACL packets the caller should write. Idempotent — if
    /// nothing has changed, returns an empty vec.
    pub fn tick(&mut self, l2cap_state: &mut L2capState) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        // Inbound first so phase-changing logic below sees the latest
        // state. The L2CAP handler only collects bytes; everything
        // else is sequenced here on the runtime thread.
        let inbound = std::mem::take(
            &mut *self
                .inbound_buffer
                .lock()
                .map_err(|_| "PBAP runtime inbound buffer poisoned".to_string())?,
        );
        let had_inbound = !inbound.is_empty();
        for (cid, bytes) in inbound {
            self.consume_inbound(cid, bytes, l2cap_state, &mut out)?;
        }
        // Shared-mux path: drain client-DLCI events surfaced by
        // RfcommState on the inbound RFCOMM channel. These carry the
        // Open transition (kick OBEX) and inbound UIH payloads
        // (advance OBEX state machine). Filter by our target DLCI so
        // we don't eat events that belong to a sibling runtime (MAP)
        // riding the same shared mux — `take_rfcomm_client_events`
        // (unfiltered) would drain everything and silently drop
        // MAP's Opened, leaving MAS stalled forever.
        let shared = match &self.phase {
            PbapRuntimePhase::AwaitingSharedDlciOpen {
                local_cid,
                target_dlci,
            }
            | PbapRuntimePhase::DrivingPbapShared {
                local_cid,
                target_dlci,
                ..
            } => Some((*local_cid, *target_dlci)),
            _ => None,
        };
        let mut had_shared_event = false;
        if let Some((cid, dlci)) = shared {
            for ev in l2cap_state.take_rfcomm_client_events_for_dlci(cid, dlci) {
                had_shared_event = true;
                self.consume_shared_event(ev, l2cap_state, &mut out)?;
            }
        }
        if had_inbound || had_shared_event {
            self.last_progress_at = Instant::now();
        }
        // Then drive any state transitions that depend on L2CAP
        // channel state (typically: a channel just transitioned to
        // Open, kick off the next protocol step).
        self.drive_outbound(l2cap_state, &mut out)?;
        // Inactivity watchdog. Only meaningful during DrivingPbap{,Shared}
        // — the SDP / RFCOMM-handshake phases progress on the L2CAP
        // configure handshake which has its own teardown, and an
        // `Idle` runtime hasn't started yet.
        let in_obex_phase = matches!(
            self.phase,
            PbapRuntimePhase::DrivingPbap { .. }
                | PbapRuntimePhase::DrivingPbapShared { .. }
                | PbapRuntimePhase::AwaitingSharedDlciOpen { .. }
                | PbapRuntimePhase::AwaitingRfcommOpen { .. }
                | PbapRuntimePhase::DrivingRfcomm { .. }
        );
        if in_obex_phase && self.last_progress_at.elapsed() >= PBAP_INACTIVITY_TIMEOUT {
            eprintln!(
                "[AokieRadio] PBAP inactivity watchdog fired after {:?} — failing PBAP so MAP can proceed",
                self.last_progress_at.elapsed()
            );
            // On the shared mux, DISC the stuck PBAP DLCI so Pixel releases its OBEX state and
            // unblocks subsequent L2CAP signaling (without DISC, MAP's SDP ConfigureRequest stalls).
            let shared_dlci = match &self.phase {
                PbapRuntimePhase::AwaitingSharedDlciOpen {
                    local_cid,
                    target_dlci,
                }
                | PbapRuntimePhase::DrivingPbapShared {
                    local_cid,
                    target_dlci,
                    ..
                } => Some((*local_cid, *target_dlci)),
                _ => None,
            };
            if let Some((cid, dlci)) = shared_dlci {
                if let Ok(disc) = l2cap_state.rfcomm_disc_client_dlci(cid, dlci) {
                    eprintln!(
                        "[AokieRadio] PBAP watchdog — sending DISC for stuck DLCI {} on shared cid 0x{:04x}",
                        dlci, cid
                    );
                    out.push(disc);
                }
            }
            self.fail(
                "PBAP inactivity watchdog: AG stopped streaming mid-fetch".to_string(),
                None,
                l2cap_state,
                &mut out,
            );
        }
        Ok(out)
    }

    // ============================================================
    // Internals.
    // ============================================================

    fn make_handler(&self) -> PsmHandler {
        let buf = self.inbound_buffer.clone();
        Arc::new(move |channel, payload| {
            buf.lock()
                .map_err(|_| "PBAP inbound buffer poisoned".to_string())?
                .push((channel.local_cid, payload.to_vec()));
            // Per-channel handler returns no auto-reply; the runtime
            // generates outbound bytes via `tick` after consuming.
            Ok(Vec::new())
        })
    }

    fn consume_inbound(
        &mut self,
        cid: u16,
        bytes: Vec<u8>,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), String> {
        // Grab phase reference. We use mem::replace gymnastics rather
        // than &mut self.phase because some transitions move owned
        // sub-states (RfcommClientState, PbapPceSession) into new
        // phase variants and the borrow checker won't let us do that
        // with a long-lived mutable borrow.
        let current_phase = std::mem::replace(&mut self.phase, PbapRuntimePhase::Done);
        match current_phase {
            PbapRuntimePhase::AwaitingSdpResponse {
                local_cid,
                mut buffer,
            } if local_cid == cid => {
                buffer.extend_from_slice(&bytes);
                match parse_service_search_attribute_response(&buffer) {
                    Ok(resp) if resp.continuation_state.is_empty() => {
                        let server_channel = match self
                            .extract_pbap_channel_from_attribute_lists(resp.attribute_lists)
                        {
                            Ok(c) => c,
                            Err(e) => {
                                self.fail(
                                    format!("PBAP SDP: {}", e),
                                    Some(local_cid),
                                    l2cap_state,
                                    out,
                                );
                                return Ok(());
                            }
                        };
                        // Politely close the SDP channel before we
                        // move on — keeps memory usage bounded and
                        // matches the spec's expected pattern.
                        if let Some(disc) = l2cap_state.disconnect_channel(local_cid) {
                            out.push(disc);
                        }
                        // Prefer riding on the existing inbound RFCOMM
                        // multiplexer if one is up — Pixel/Bluedroid
                        // silently drops a second L2CAP/PSM 0x0003
                        // session per peer (see project memory
                        // `bluedroid_one_rfcomm_session`). Symptom in
                        // logs: our outbound ConnectionRequest gets a
                        // success ConnectionResponse but the configure
                        // handshake stalls forever.
                        // Falls back to opening our own L2CAP+RFCOMM
                        // session only when no shared mux exists, so
                        // the unit-test harness (which doesn't simulate
                        // the inbound HFP path) keeps exercising the
                        // legacy state machine. Mirrors the same logic
                        // already in `map_runtime.rs`.
                        if let Some(shared_cid) =
                            l2cap_state.find_open_rfcomm_channel(self.connection_handle)
                        {
                            match l2cap_state.rfcomm_attach_client_dlci(shared_cid, server_channel)
                            {
                                Ok((target_dlci, pn_acl)) => {
                                    out.push(pn_acl);
                                    eprintln!(
                                        "[AokieRadio] PBAP SDP done — PSE server_channel={} → target dlci={} on shared mux cid 0x{:04x}",
                                        server_channel, target_dlci, shared_cid
                                    );
                                    self.phase = PbapRuntimePhase::AwaitingSharedDlciOpen {
                                        local_cid: shared_cid,
                                        target_dlci,
                                    };
                                    return Ok(());
                                }
                                Err(e) => {
                                    self.fail(
                                        format!("PBAP shared DLCI attach: {}", e),
                                        None,
                                        l2cap_state,
                                        out,
                                    );
                                    return Ok(());
                                }
                            }
                        }
                        let (rfcomm_cid, conn_req) = l2cap_state
                            .open_outbound_channel_with_handler(
                                self.connection_handle,
                                PSM_RFCOMM,
                                Some(self.make_handler()),
                            );
                        out.push(conn_req);
                        eprintln!(
                            "[AokieRadio] PBAP SDP done — PSE RFCOMM channel {} on cid 0x{:04x} (no shared mux)",
                            server_channel, rfcomm_cid
                        );
                        self.phase = PbapRuntimePhase::AwaitingRfcommOpen {
                            server_channel,
                            local_cid: rfcomm_cid,
                        };
                        return Ok(());
                    }
                    Ok(_) => {
                        self.fail(
                            "PBAP SDP: continuation state present (unexpected for our small query)"
                                .to_string(),
                            Some(local_cid),
                            l2cap_state,
                            out,
                        );
                        return Ok(());
                    }
                    Err(_) => {
                        // Need more bytes — restore phase and wait.
                        self.phase = PbapRuntimePhase::AwaitingSdpResponse { local_cid, buffer };
                        return Ok(());
                    }
                }
            }
            PbapRuntimePhase::DrivingRfcomm {
                local_cid,
                mut client,
            } if local_cid == cid => {
                let responses = client.handle_packet(&bytes)?;
                for r in responses {
                    out.push(l2cap_state.send_on_channel(local_cid, &r)?);
                }
                for ev in client.take_events() {
                    match ev {
                        RfcommClientEvent::Opened => {
                            // RFCOMM is up — fire the first OBEX
                            // CONNECT request and pivot to DrivingPbap.
                            let mut tmp_session = PbapPceSession::new();
                            if let Some(connect_req) = tmp_session.next_request() {
                                let uih = client.build_outbound_uih(&connect_req)?;
                                out.push(l2cap_state.send_on_channel(local_cid, &uih)?);
                                self.phase = PbapRuntimePhase::DrivingPbap {
                                    local_cid,
                                    client,
                                    session: tmp_session,
                                    obex_buffer: Vec::new(),
                                };
                                return Ok(());
                            } else {
                                self.fail(
                                    "PBAP session refused to produce CONNECT request".to_string(),
                                    Some(local_cid),
                                    l2cap_state,
                                    out,
                                );
                                return Ok(());
                            }
                        }
                        RfcommClientEvent::Closed => {
                            self.fail(
                                "RFCOMM closed before PBAP session opened".to_string(),
                                Some(local_cid),
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                        RfcommClientEvent::Failed(reason) => {
                            self.fail(
                                format!("RFCOMM client failed: {}", reason),
                                Some(local_cid),
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                        RfcommClientEvent::Payload(_) => {
                            // Out-of-phase — payloads shouldn't arrive
                            // during DrivingRfcomm. Drop silently.
                        }
                    }
                }
                self.phase = PbapRuntimePhase::DrivingRfcomm { local_cid, client };
                return Ok(());
            }
            PbapRuntimePhase::DrivingPbap {
                local_cid,
                mut client,
                mut session,
                mut obex_buffer,
            } if local_cid == cid => {
                let responses = client.handle_packet(&bytes)?;
                for r in responses {
                    out.push(l2cap_state.send_on_channel(local_cid, &r)?);
                }
                for ev in client.take_events() {
                    match ev {
                        RfcommClientEvent::Payload(obex_bytes) => {
                            obex_buffer.extend_from_slice(&obex_bytes);
                            // Drain complete OBEX packets out of the
                            // reassembly buffer and feed them to the
                            // session.
                            loop {
                                let take = match try_take_obex_packet(&obex_buffer) {
                                    Ok(Some(n)) => n,
                                    Ok(None) => break,
                                    Err(e) => {
                                        self.fail(
                                            format!("OBEX packet header malformed: {}", e),
                                            Some(local_cid),
                                            l2cap_state,
                                            out,
                                        );
                                        return Ok(());
                                    }
                                };
                                let packet_bytes: Vec<u8> = obex_buffer.drain(..take).collect();
                                let fixed = expected_fixed_payload(session.state());
                                let parsed = match Packet::parse(&packet_bytes, fixed) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        self.fail(
                                            format!("PBAP OBEX parse failed: {}", e),
                                            Some(local_cid),
                                            l2cap_state,
                                            out,
                                        );
                                        return Ok(());
                                    }
                                };
                                if let Some(next_req) = session.handle_response(&parsed) {
                                    let uih = client.build_outbound_uih(&next_req)?;
                                    out.push(l2cap_state.send_on_channel(local_cid, &uih)?);
                                }
                                // Check terminal session states.
                                match session.state() {
                                    PbapState::Done => {
                                        // Politely close the channel.
                                        let disc_uih = client.build_target_disc();
                                        if let Ok(uih_acl) =
                                            l2cap_state.send_on_channel(local_cid, &disc_uih)
                                        {
                                            out.push(uih_acl);
                                        }
                                        if let Some(l2cap_disc) =
                                            l2cap_state.disconnect_channel(local_cid)
                                        {
                                            out.push(l2cap_disc);
                                        }
                                        // Surface contacts.
                                        let contacts: Vec<Contact> = session.contacts().to_vec();
                                        self.pending_events
                                            .push(PbapRuntimeEvent::ContactsFetched(contacts));
                                        self.phase = PbapRuntimePhase::Done;
                                        return Ok(());
                                    }
                                    PbapState::Failed(reason) => {
                                        let reason = reason.clone();
                                        self.fail(
                                            format!("PBAP session failed: {}", reason),
                                            Some(local_cid),
                                            l2cap_state,
                                            out,
                                        );
                                        return Ok(());
                                    }
                                    _ => {}
                                }
                            }
                        }
                        RfcommClientEvent::Closed => {
                            self.fail(
                                "RFCOMM closed mid-PBAP session".to_string(),
                                Some(local_cid),
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                        RfcommClientEvent::Failed(reason) => {
                            self.fail(
                                format!("RFCOMM client failed: {}", reason),
                                Some(local_cid),
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                        RfcommClientEvent::Opened => {
                            // Already opened — duplicate event is fine
                            // to ignore.
                        }
                    }
                }
                self.phase = PbapRuntimePhase::DrivingPbap {
                    local_cid,
                    client,
                    session,
                    obex_buffer,
                };
                return Ok(());
            }
            other => {
                // Inbound bytes for a stale cid (channel was already
                // disconnected) or out-of-phase data — drop. Restore
                // the phase we replaced.
                self.phase = other;
                return Ok(());
            }
        }
    }

    fn drive_outbound(
        &mut self,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), String> {
        match &self.phase {
            PbapRuntimePhase::AwaitingSdpOpen { local_cid } => {
                let cid = *local_cid;
                if let Some(channel) = l2cap_state.channel(cid) {
                    if channel.state == ChannelState::Open {
                        let req = build_service_search_attribute_request(
                            self.sdp_transaction_id,
                            UUID_PBAP_PSE,
                            0xffff,
                        );
                        let acl = l2cap_state.send_on_channel(cid, &req)?;
                        out.push(acl);
                        self.phase = PbapRuntimePhase::AwaitingSdpResponse {
                            local_cid: cid,
                            buffer: Vec::new(),
                        };
                    }
                }
            }
            PbapRuntimePhase::AwaitingRfcommOpen {
                server_channel,
                local_cid,
            } => {
                let cid = *local_cid;
                let channel_num = *server_channel;
                if let Some(channel) = l2cap_state.channel(cid) {
                    if channel.state == ChannelState::Open {
                        let mut client = RfcommClientState::new(channel_num);
                        let sabm = client.kickoff()?;
                        let acl = l2cap_state.send_on_channel(cid, &sabm)?;
                        out.push(acl);
                        self.phase = PbapRuntimePhase::DrivingRfcomm {
                            local_cid: cid,
                            client,
                        };
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Handle a `ClientDlciEvent` from the shared inbound RFCOMM mux.
    /// Mirrors `map_runtime::consume_shared_event` — Pixel/Bluedroid
    /// only allow one RFCOMM session per peer, so PBAP attaches a
    /// new client DLCI to the existing inbound mux instead of opening
    /// its own L2CAP/PSM 0x0003 channel.
    fn consume_shared_event(
        &mut self,
        ev: ClientDlciEvent,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), String> {
        match ev {
            ClientDlciEvent::Opened { dlci } => {
                let phase = std::mem::replace(&mut self.phase, PbapRuntimePhase::Done);
                let PbapRuntimePhase::AwaitingSharedDlciOpen {
                    local_cid,
                    target_dlci,
                } = phase
                else {
                    self.phase = phase;
                    return Ok(());
                };
                if dlci != target_dlci {
                    self.phase = PbapRuntimePhase::AwaitingSharedDlciOpen {
                        local_cid,
                        target_dlci,
                    };
                    return Ok(());
                }
                let mut session = PbapPceSession::new();
                let Some(connect_req) = session.next_request() else {
                    self.fail(
                        "PBAP session refused to produce CONNECT request".to_string(),
                        None,
                        l2cap_state,
                        out,
                    );
                    return Ok(());
                };
                let acl = l2cap_state.rfcomm_send_uih_on_client_dlci(
                    local_cid,
                    target_dlci,
                    &connect_req,
                )?;
                out.push(acl);
                eprintln!(
                    "[AokieRadio] PBAP shared DLCI {} OPEN — sending OBEX CONNECT",
                    target_dlci
                );
                self.phase = PbapRuntimePhase::DrivingPbapShared {
                    local_cid,
                    target_dlci,
                    session,
                    obex_buffer: Vec::new(),
                };
                Ok(())
            }
            ClientDlciEvent::Failed { dlci: _, reason } => {
                self.fail(
                    format!("RFCOMM shared DLCI failed: {}", reason),
                    None,
                    l2cap_state,
                    out,
                );
                Ok(())
            }
            ClientDlciEvent::Payload { dlci, payload } => {
                let phase = std::mem::replace(&mut self.phase, PbapRuntimePhase::Done);
                let PbapRuntimePhase::DrivingPbapShared {
                    local_cid,
                    target_dlci,
                    mut session,
                    mut obex_buffer,
                } = phase
                else {
                    self.phase = phase;
                    return Ok(());
                };
                if dlci != target_dlci {
                    self.phase = PbapRuntimePhase::DrivingPbapShared {
                        local_cid,
                        target_dlci,
                        session,
                        obex_buffer,
                    };
                    return Ok(());
                }
                let prior_buf_len = obex_buffer.len();
                obex_buffer.extend_from_slice(&payload);
                loop {
                    let take = match try_take_obex_packet(&obex_buffer) {
                        Ok(Some(n)) => n,
                        Ok(None) => {
                            // Throttled progress log: when reassembling
                            // a multi-UIH OBEX packet, log once per
                            // ~1KB so we have evidence if the AG goes
                            // silent mid-stream without spamming the
                            // log per-frame in the steady-state case.
                            if obex_buffer.len() >= 3
                                && obex_buffer.len() / 1024 != prior_buf_len / 1024
                            {
                                let declared = u16::from_be_bytes([obex_buffer[1], obex_buffer[2]]);
                                eprintln!(
                                    "[AokieRadio] PBAP OBEX reassembling on dlci {} — buffered {}B / declared {}B (opcode 0x{:02x})",
                                    target_dlci,
                                    obex_buffer.len(),
                                    declared,
                                    obex_buffer[0]
                                );
                            }
                            break;
                        }
                        Err(e) => {
                            self.fail(
                                format!("OBEX packet header malformed: {}", e),
                                None,
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                    };
                    let packet_bytes: Vec<u8> = obex_buffer.drain(..take).collect();
                    let fixed = expected_fixed_payload(session.state());
                    let parsed = match Packet::parse(&packet_bytes, fixed) {
                        Ok(p) => p,
                        Err(e) => {
                            self.fail(
                                format!("PBAP OBEX parse failed: {}", e),
                                None,
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                    };
                    eprintln!(
                        "[AokieRadio] PBAP OBEX response opcode 0x{:02x} ({}B) — state before: {:?}",
                        parsed.opcode,
                        packet_bytes.len(),
                        session.state()
                    );
                    if let Some(next_req) = session.handle_response(&parsed) {
                        let uih = l2cap_state.rfcomm_send_uih_on_client_dlci(
                            local_cid,
                            target_dlci,
                            &next_req,
                        )?;
                        out.push(uih);
                        eprintln!(
                            "[AokieRadio] PBAP outbound on dlci {} — {}B (state after: {:?})",
                            target_dlci,
                            next_req.len(),
                            session.state()
                        );
                    }
                    match session.state() {
                        PbapState::Done => {
                            // Shared-mux teardown: close the DLCI we
                            // attached, but DO NOT touch the L2CAP
                            // channel — it's the inbound HFP mux,
                            // still in use by HFP / MAP / future PBAP
                            // ops.
                            if let Ok(disc) =
                                l2cap_state.rfcomm_disc_client_dlci(local_cid, target_dlci)
                            {
                                out.push(disc);
                            }
                            let contacts: Vec<Contact> = session.contacts().to_vec();
                            self.pending_events
                                .push(PbapRuntimeEvent::ContactsFetched(contacts));
                            self.phase = PbapRuntimePhase::Done;
                            return Ok(());
                        }
                        PbapState::Failed(reason) => {
                            let reason = reason.clone();
                            self.fail(
                                format!("PBAP session failed: {}", reason),
                                None,
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                self.phase = PbapRuntimePhase::DrivingPbapShared {
                    local_cid,
                    target_dlci,
                    session,
                    obex_buffer,
                };
                Ok(())
            }
        }
    }

    fn extract_pbap_channel_from_attribute_lists(
        &self,
        attribute_lists: &[u8],
    ) -> Result<u8, String> {
        let records = iter_records(attribute_lists)?;
        for record in records {
            if let Some(channel) = extract_rfcomm_channel(record)? {
                return Ok(channel);
            }
        }
        Err("PBAP PSE service record did not advertise an RFCOMM channel".to_string())
    }

    fn fail(
        &mut self,
        reason: String,
        active_cid: Option<u16>,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) {
        // Tear down any L2CAP channel we were driving so the AG isn't
        // left holding it open. We don't await the response — the
        // runtime is now Failed and won't process further inbound.
        if let Some(cid) = active_cid {
            if let Some(packet) = l2cap_state.disconnect_channel(cid) {
                out.push(packet);
            }
        }
        self.pending_events
            .push(PbapRuntimeEvent::Failed(reason.clone()));
        self.phase = PbapRuntimePhase::Failed(reason);
    }
}

/// Inspect the OBEX byte stream and return the number of bytes that
/// constitute the next complete packet, or `None` if more bytes are
/// needed. `Err` if the buffer's first 3 bytes form a structurally
/// invalid header (length < 3).
fn try_take_obex_packet(buffer: &[u8]) -> Result<Option<usize>, String> {
    if buffer.len() < 3 {
        return Ok(None);
    }
    let len = u16::from_be_bytes([buffer[1], buffer[2]]) as usize;
    if len < 3 {
        return Err(format!(
            "OBEX packet declares length {} (must be >= 3 for the header)",
            len
        ));
    }
    if buffer.len() < len {
        return Ok(None);
    }
    Ok(Some(len))
}

/// What `FixedPayload` kind to pass to `Packet::parse` for the
/// response shape we're currently expecting. Maps directly off the
/// PBAP session state — no other PBAP-specific context needed.
fn expected_fixed_payload(state: &PbapState) -> FixedPayload {
    match state {
        // CONNECT response carries the same fixed payload shape as
        // CONNECT request: version + flags + max_packet_length.
        PbapState::AwaitingConnect => FixedPayload::Connect,
        // SETPATH responses are bare OK with no fixed payload — only
        // the request side carries the SETPATH flags + constants
        // byte.
        _ => FixedPayload::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aokie_radio::l2cap::{
        build_acl_packet, build_basic_frame, ACL_BROADCAST_POINT_TO_POINT,
        ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE, CID_SIGNALING,
    };
    use crate::aokie_radio::obex::{build_connect_response, RSP_OK};

    #[test]
    fn try_take_obex_packet_returns_none_until_full_packet_available() {
        // 3-byte header declares length 6: opcode=0xa0, len=0x0006
        let header = [0xa0, 0x00, 0x06];
        // Not enough bytes yet.
        assert_eq!(try_take_obex_packet(&header[..2]).unwrap(), None);
        // Header alone declares length=6 but buffer is only 3.
        assert_eq!(try_take_obex_packet(&header).unwrap(), None);
        // Append 3 more bytes — total now 6 = declared length.
        let mut buf = header.to_vec();
        buf.extend_from_slice(&[0xde, 0xad, 0xbe]);
        assert_eq!(try_take_obex_packet(&buf).unwrap(), Some(6));
        // Trailing extra bytes don't affect the take size.
        buf.extend_from_slice(&[0xff, 0xee]);
        assert_eq!(try_take_obex_packet(&buf).unwrap(), Some(6));
    }

    #[test]
    fn try_take_obex_packet_errors_on_pathological_short_length() {
        let bad = [0xa0, 0x00, 0x02]; // length 2 < 3-byte header
        assert!(try_take_obex_packet(&bad).is_err());
    }

    #[test]
    fn pbap_runtime_start_opens_outbound_sdp_channel_and_emits_connection_request() {
        let mut runtime = PbapRuntime::new(0x002a);
        let mut l2cap = L2capState::new();
        let acls = runtime.start(&mut l2cap).unwrap();
        assert_eq!(acls.len(), 1, "one ConnectionRequest");
        // Channel registered, but in Configuring (no peer ACK yet).
        assert_eq!(l2cap.channel_count(), 1);

        // Calling start twice errors.
        assert!(runtime.start(&mut l2cap).is_err());
    }

    #[test]
    fn pbap_runtime_walks_full_flow_to_contacts_fetched() {
        // End-to-end happy path: start → SDP open → SDP request →
        // SDP response → RFCOMM open → RFCOMM handshake → OBEX
        // CONNECT → SETPATH telecom → SETPATH pb → GET phonebook
        // (single-packet for simplicity) → DISCONNECT → Done.
        let connection_handle = 0x002a;
        let mut runtime = PbapRuntime::new(connection_handle);
        let mut l2cap = L2capState::new();

        // Phase: AwaitingSdpOpen
        let sdp_local_cid = 0x0040; // FIRST_DYNAMIC_CID; deterministic.
        let _ = runtime.start(&mut l2cap).unwrap();

        // Drive SDP L2CAP to Open. configure_id = 0x81 is what
        // L2capState.handle_connection_response allocates after the
        // 0x80 we used for ConnectionRequest.
        walk_outbound_l2cap_to_open(&mut l2cap, connection_handle, sdp_local_cid, 0x0070, 0x81);
        // Tick: should send SDP request.
        let acls = runtime.tick(&mut l2cap).unwrap();
        assert_eq!(
            acls.len(),
            1,
            "tick after SDP Open emits ServiceSearchAttributeRequest"
        );
        // Phase should now be AwaitingSdpResponse.
        assert!(matches!(
            runtime.phase,
            PbapRuntimePhase::AwaitingSdpResponse { .. }
        ));

        // Inject SDP response: PBAP record advertising RFCOMM channel 19.
        // Inbound basic frame cid = OUR local_cid; the AG addresses
        // to the cid it learned from our ConnectionRequest source_cid.
        let sdp_response = build_pbap_sdp_response(0x0001, 19);
        let basic = build_basic_frame(sdp_local_cid, &sdp_response);
        let acl = build_acl_packet(
            connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        );
        // Make the channel route the response into our handler.
        let _ = l2cap.handle_acl_packet(&acl).unwrap();

        // Tick: should disconnect SDP, open RFCOMM L2CAP, transition to AwaitingRfcommOpen.
        let acls = runtime.tick(&mut l2cap).unwrap();
        // 2 outbound: SDP DisconnectionRequest + RFCOMM ConnectionRequest.
        assert_eq!(acls.len(), 2);
        let phase_str = format!("{:?}", runtime.phase);
        assert!(
            matches!(
                runtime.phase,
                PbapRuntimePhase::AwaitingRfcommOpen {
                    server_channel: 19,
                    ..
                }
            ),
            "phase = {}",
            phase_str
        );

        // RFCOMM channel local_cid should be next dynamic CID. The SDP
        // channel was removed, so allocator returns the next free
        // value: 0x0041 in our deterministic test world.
        let rfcomm_local_cid = match runtime.phase {
            PbapRuntimePhase::AwaitingRfcommOpen { local_cid, .. } => local_cid,
            _ => unreachable!(),
        };
        assert_eq!(rfcomm_local_cid, 0x0041);

        // Drive RFCOMM L2CAP to Open.
        walk_outbound_l2cap_to_open(
            &mut l2cap,
            connection_handle,
            rfcomm_local_cid,
            0x0080,
            0x84, // SDP used 0x80/0x81/0x82; RFCOMM ConnReq=0x83, ConfigReq=0x84.
        );
        // Tick: should kick off RFCOMM SABM.
        let acls = runtime.tick(&mut l2cap).unwrap();
        assert_eq!(acls.len(), 1, "RFCOMM SABM after L2CAP Open");
        assert!(matches!(
            runtime.phase,
            PbapRuntimePhase::DrivingRfcomm { .. }
        ));

        // Drive RFCOMM client through UA/PN/SABM/MSC handshake by
        // injecting the AG's responses.
        feed_rfcomm_to_client(
            &mut runtime,
            &mut l2cap,
            connection_handle,
            rfcomm_local_cid,
            19,
        );

        // Should now be in DrivingPbap.
        let phase_str = format!("{:?}", runtime.phase);
        assert!(
            matches!(runtime.phase, PbapRuntimePhase::DrivingPbap { .. }),
            "phase = {}",
            phase_str
        );

        // Inject OBEX responses for CONNECT/SETPATH/SETPATH/GET/DISCONNECT.
        feed_pbap_obex_responses(
            &mut runtime,
            &mut l2cap,
            connection_handle,
            rfcomm_local_cid,
            19,
        );

        // Should now be in Done with a ContactsFetched event.
        let events = runtime.take_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PbapRuntimeEvent::ContactsFetched(_))),
            "expected ContactsFetched, got {:?}",
            events
        );
        assert!(runtime.is_done());
    }

    /// Walk an outbound L2CAP channel from Configuring to Open by
    /// injecting the AG's ConnectionResponse + ConfigureRequest +
    /// ConfigureResponse over `l2cap_state`.
    fn walk_outbound_l2cap_to_open(
        l2cap: &mut L2capState,
        connection_handle: u16,
        local_cid: u16,
        remote_cid: u16,
        configure_id: u8,
    ) {
        let cid_bytes = local_cid.to_le_bytes();
        let rcid_bytes = remote_cid.to_le_bytes();
        // ConnectionResponse identifier echoes the one we sent on
        // ConnectionRequest. Signaling-id allocator runs:
        //   SDP   ConnReq = 0x80, ConfigReq = 0x81, DiscReq = 0x82
        //   RFCOMM ConnReq = 0x83, ConfigReq = 0x84
        // so the inverse for the second channel is 0x83.
        let outbound_connect_id = match local_cid {
            0x0040 => 0x80,
            0x0041 => 0x83,
            _ => 0x80,
        };
        let conn_resp = build_acl_packet(
            connection_handle,
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

        // Peer ConfigureRequest at our local cid.
        let cfg_req = build_acl_packet(
            connection_handle,
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

        // Peer ConfigureResponse for our ConfigureRequest.
        let cfg_resp = build_acl_packet(
            connection_handle,
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

    /// Feed AG-side RFCOMM frames into the runtime to walk
    /// RfcommClientState from kickoff to Open.
    fn feed_rfcomm_to_client(
        runtime: &mut PbapRuntime,
        l2cap: &mut L2capState,
        connection_handle: u16,
        local_cid: u16,
        server_channel: u8,
    ) {
        use crate::aokie_radio::rfcomm::{
            build_modem_status_command, build_modem_status_response,
            build_parameter_negotiation_response, build_ua, build_uih, server_channel_dlci,
            RFCOMM_DLCI_MULTIPLEXER, RFCOMM_LOCAL_MODEM_STATUS,
        };
        let target_dlci = server_channel_dlci(server_channel, true);
        // 1. UA on multiplexer DLCI.
        feed_rfcomm(
            runtime,
            l2cap,
            connection_handle,
            local_cid,
            build_ua(RFCOMM_DLCI_MULTIPLEXER, true),
        );
        // 2. PN response.
        feed_rfcomm(
            runtime,
            l2cap,
            connection_handle,
            local_cid,
            build_uih(
                RFCOMM_DLCI_MULTIPLEXER,
                false,
                None,
                &build_parameter_negotiation_response(target_dlci, 7, 127, 7),
            ),
        );
        // 3. UA on target DLCI.
        feed_rfcomm(
            runtime,
            l2cap,
            connection_handle,
            local_cid,
            build_ua(target_dlci, true),
        );
        // 4. MSC RSP for our MSC CMD.
        feed_rfcomm(
            runtime,
            l2cap,
            connection_handle,
            local_cid,
            build_uih(
                RFCOMM_DLCI_MULTIPLEXER,
                false,
                None,
                &build_modem_status_response(target_dlci, RFCOMM_LOCAL_MODEM_STATUS),
            ),
        );
        // 5. Peer's MSC CMD; this triggers Opened event and OBEX CONNECT.
        feed_rfcomm(
            runtime,
            l2cap,
            connection_handle,
            local_cid,
            build_uih(
                RFCOMM_DLCI_MULTIPLEXER,
                false,
                None,
                &build_modem_status_command(target_dlci, RFCOMM_LOCAL_MODEM_STATUS),
            ),
        );
    }

    fn feed_rfcomm(
        runtime: &mut PbapRuntime,
        l2cap: &mut L2capState,
        connection_handle: u16,
        local_cid: u16,
        rfcomm_frame: Vec<u8>,
    ) {
        // Inbound basic-frame cid = our local_cid. (AG addresses to
        // the cid it learned from our ConnectionRequest source_cid.)
        let basic = build_basic_frame(local_cid, &rfcomm_frame);
        let acl = build_acl_packet(
            connection_handle,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        );
        let _ = l2cap.handle_acl_packet(&acl).unwrap();
        let _ = runtime.tick(l2cap).unwrap();
    }

    fn feed_pbap_obex_responses(
        runtime: &mut PbapRuntime,
        l2cap: &mut L2capState,
        connection_handle: u16,
        local_cid: u16,
        server_channel: u8,
    ) {
        use crate::aokie_radio::rfcomm::{build_uih, server_channel_dlci};
        // Helper: wrap OBEX bytes in a UIH frame on the target DLCI
        // and feed through L2CAP.
        let target_dlci = server_channel_dlci(server_channel, true);
        let send_obex = |runtime: &mut PbapRuntime, l2cap: &mut L2capState, payload: &[u8]| {
            // The peer is the responder side, sending UIH responses
            // on the target DLCI. CR=false from the responder for
            // commands; for OBEX bytes we use whatever bit; the
            // RfcommClientState ignores CR semantics for payload.
            let frame = build_uih(target_dlci, false, None, payload);
            feed_rfcomm(runtime, l2cap, connection_handle, local_cid, frame);
        };

        // 1. CONNECT response: OK with Connection-Id quad header.
        let connect_resp = build_connect_response(RSP_OK, 0x4000, Some(0xdeadbeef), None);
        send_obex(runtime, l2cap, &connect_resp);
        // 2. SETPATH telecom response: OK.
        let setpath_resp = build_simple_ok_response();
        send_obex(runtime, l2cap, &setpath_resp);
        // 3. SETPATH pb response: OK.
        send_obex(runtime, l2cap, &setpath_resp);
        // 4. GET phonebook response: OK with EndOfBody containing one
        //    minimal vCard.
        let phonebook_body = b"BEGIN:VCARD\r\nVERSION:2.1\r\nN:Doe;Jane\r\nFN:Jane Doe\r\nTEL:+15551234567\r\nEND:VCARD\r\n";
        let get_resp = build_ok_with_endofbody(phonebook_body);
        send_obex(runtime, l2cap, &get_resp);
        // 5. DISCONNECT response: OK.
        send_obex(runtime, l2cap, &setpath_resp);
    }

    /// Build a minimal OK response: opcode 0xA0, length 3, no headers.
    fn build_simple_ok_response() -> Vec<u8> {
        vec![0xa0, 0x00, 0x03]
    }

    /// Build an OK response containing an EndOfBody header with the
    /// supplied payload. Hand-encoded to avoid pulling in more obex
    /// internals here.
    fn build_ok_with_endofbody(body: &[u8]) -> Vec<u8> {
        // EndOfBody header: id 0x49 (ByteSeq), 2-byte BE length
        // including the 3-byte header, then the payload.
        let header_total_len = 3 + body.len();
        let mut packet = Vec::new();
        packet.push(0xa0); // OK with final bit
                           // Reserve 2 bytes for total packet length, fill in below.
        packet.push(0x00);
        packet.push(0x00);
        packet.push(0x49); // EndOfBody header id
        packet.extend_from_slice(&(header_total_len as u16).to_be_bytes());
        packet.extend_from_slice(body);
        let total = packet.len() as u16;
        packet[1] = (total >> 8) as u8;
        packet[2] = (total & 0xff) as u8;
        packet
    }

    /// Build an SDP ServiceSearchAttributeResponse advertising one
    /// PBAP PSE record on the given RFCOMM channel. Hand-encoded so
    /// the test doesn't have to fight the more general SDP encoder
    /// API (which takes Cow-style `&[u8]` payloads with
    /// pre-encoded children).
    fn build_pbap_sdp_response(transaction_id: u16, rfcomm_channel: u8) -> Vec<u8> {
        // Build the ProtocolDescriptorList: nested sequences of
        // (L2CAP UUID) followed by (RFCOMM UUID + channel).
        // Inner sequence 1: 19 01 00            (UUID16 L2CAP)
        let l2cap_seq: Vec<u8> = vec![0x35, 0x03, 0x19, 0x01, 0x00];
        // Inner sequence 2: 19 00 03 08 <ch>    (UUID16 RFCOMM + UINT8 channel)
        let rfcomm_seq: Vec<u8> = vec![0x35, 0x05, 0x19, 0x00, 0x03, 0x08, rfcomm_channel];
        let pdl_payload: Vec<u8> = [l2cap_seq, rfcomm_seq].concat();
        // ProtocolDescriptorList sequence: 35 <len> <pdl_payload>
        let mut pdl_seq: Vec<u8> = vec![0x35, pdl_payload.len() as u8];
        pdl_seq.extend_from_slice(&pdl_payload);
        // Attribute pair: 09 00 04 (UINT16 ATTR_PROTOCOL_DESCRIPTOR_LIST=0x0004) + pdl_seq.
        let mut attr_pair: Vec<u8> = vec![0x09, 0x00, 0x04];
        attr_pair.extend_from_slice(&pdl_seq);
        // Wrap in attribute_list sequence (one record).
        let mut attr_list_seq: Vec<u8> = vec![0x35, attr_pair.len() as u8];
        attr_list_seq.extend_from_slice(&attr_pair);
        // Outer "sequence of records" sequence — just the one record.
        let mut outer_seq: Vec<u8> = vec![0x35, attr_list_seq.len() as u8];
        outer_seq.extend_from_slice(&attr_list_seq);
        // SDP PDU.
        let mut pdu: Vec<u8> = Vec::new();
        pdu.push(0x07); // ServiceSearchAttributeResponse
        pdu.extend_from_slice(&transaction_id.to_be_bytes());
        let attr_byte_count = outer_seq.len() as u16;
        let parameter_length: u16 = 2 + attr_byte_count + 1;
        pdu.extend_from_slice(&parameter_length.to_be_bytes());
        pdu.extend_from_slice(&attr_byte_count.to_be_bytes());
        pdu.extend_from_slice(&outer_seq);
        pdu.push(0x00); // continuation state length 0
        pdu
    }

    #[test]
    fn pbap_runtime_fails_cleanly_when_sdp_response_lacks_rfcomm_channel() {
        let mut runtime = PbapRuntime::new(0x002a);
        let mut l2cap = L2capState::new();
        let _ = runtime.start(&mut l2cap).unwrap();
        walk_outbound_l2cap_to_open(&mut l2cap, 0x002a, 0x0040, 0x0070, 0x81);
        let _ = runtime.tick(&mut l2cap).unwrap(); // sends SDP request

        // Inject an SDP response that has one record with no
        // ProtocolDescriptorList attribute. PbapRuntime should fail.
        // Empty inner record: 35 00 (sequence with zero-byte payload).
        let inner_record: Vec<u8> = vec![0x35, 0x00];
        // Outer "sequence of records" containing the empty record.
        let mut outer: Vec<u8> = vec![0x35, inner_record.len() as u8];
        outer.extend_from_slice(&inner_record);
        let mut pdu: Vec<u8> = vec![0x07, 0x00, 0x01];
        let attr_byte_count = outer.len() as u16;
        let parameter_length: u16 = 2 + attr_byte_count + 1;
        pdu.extend_from_slice(&parameter_length.to_be_bytes());
        pdu.extend_from_slice(&attr_byte_count.to_be_bytes());
        pdu.extend_from_slice(&outer);
        pdu.push(0x00);
        let basic = build_basic_frame(0x0040, &pdu);
        let acl = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &basic,
        );
        let _ = l2cap.handle_acl_packet(&acl).unwrap();
        let _ = runtime.tick(&mut l2cap).unwrap();

        let events = runtime.take_events();
        assert!(
            events.iter().any(|e| matches!(e, PbapRuntimeEvent::Failed(reason) if reason.contains("did not advertise"))),
            "expected Failed event, got {:?}",
            events
        );
        assert!(runtime.is_failed());
    }
}
