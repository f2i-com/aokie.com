//! MAP runtime — orchestrates SDP discovery → outbound L2CAP → RFCOMM
//! client → OBEX/MAS for MAP operations against the AG.
//!
//! Phase 4b of the SMS auto-reply plan, with Phase 4g.d adding pooled
//! sessions. Default behaviour (no `enable_pooling()` call) mirrors
//! the original "construct a fresh runtime per op" model; in pooled
//! mode the same runtime accepts multiple back-to-back ops without
//! tearing down the OBEX/RFCOMM/L2CAP connection between them.
//!
//! Lifecycle (one-shot):
//! 1. `MapRuntime::new(connection_handle, operation, mas_instance_id)`.
//! 2. `start(l2cap_state)` — opens the SDP outbound channel.
//! 3. After every `l2cap_state.handle_acl_packet`, call `tick(...)`,
//!    drain the produced ACL packets, write them via the transport.
//! 4. `take_events()` surfaces `OperationCompleted(output)` or `Failed`.
//! 5. After Done/Failed no further calls are valid.
//!
//! Lifecycle (pooled — Phase 4g.d):
//! 1. Construct as above, then `enable_pooling()` BEFORE `start()`.
//! 2. Drive the first op exactly the same way; on completion the
//!    runtime parks in `MapRuntimePhase::Resting` instead of
//!    transitioning to Done.
//! 3. Caller checks `is_resting()`. To queue another op:
//!    `start_next_op(operation, l2cap_state)` — emits the SETPATH
//!    chain (or op-specific request) over the existing connection.
//!    To tear down: `request_disconnect(l2cap_state)`.

#![allow(dead_code)] // Phase 4b — orchestrator integration follows in 4e.

use std::sync::{Arc, Mutex as StdMutex};

use super::l2cap::{ChannelState, L2capState, PsmHandler, PSM_RFCOMM, PSM_SDP};
use super::map_mas::{MasMceSession, MasState, Operation, OperationOutput};
use super::obex::{FixedPayload, Packet};
use super::rfcomm::{ClientDlciEvent, RfcommClientEvent, RfcommClientState};
use super::sdp_client::{
    build_service_search_attribute_request, extract_rfcomm_channel, iter_records,
    parse_service_search_attribute_response, UUID_MAP_MAS,
};

#[derive(Debug, Clone)]
pub enum MapRuntimeEvent {
    /// MAS operation finished cleanly. The output carries the
    /// listing, the message body, or a "push acknowledged" flag —
    /// see `MasMceSession::OperationOutput` for shape.
    OperationCompleted(OperationOutput),
    /// Anything went wrong; the runtime is now Failed and any
    /// channels it opened have been disconnect-requested.
    Failed(String),
}

#[derive(Debug)]
enum MapRuntimePhase {
    Idle,
    AwaitingSdpOpen {
        local_cid: u16,
    },
    AwaitingSdpResponse {
        local_cid: u16,
        buffer: Vec<u8>,
    },
    AwaitingRfcommOpen {
        server_channel: u8,
        local_cid: u16,
    },
    DrivingRfcomm {
        local_cid: u16,
        client: RfcommClientState,
    },
    DrivingMas {
        local_cid: u16,
        client: RfcommClientState,
        session: MasMceSession,
        obex_buffer: Vec<u8>,
    },
    /// Shared-mux variant: PN/SABM/MSC for the MAS DLCI happen *on the
    /// inbound HFP RFCOMM channel*'s multiplexer (Bluedroid refuses a
    /// second L2CAP/PSM 0x0003 session per peer). `local_cid` is the
    /// inbound channel, NOT one we opened. We don't own
    /// `RfcommClientState` here — RfcommState (inside L2capState's
    /// channel) drives the DLCI handshake and surfaces ClientDlciEvent
    /// via the L2capState's `take_rfcomm_client_events`.
    AwaitingSharedDlciOpen {
        local_cid: u16,
        target_dlci: u8,
    },
    DrivingMasShared {
        local_cid: u16,
        target_dlci: u8,
        session: MasMceSession,
        obex_buffer: Vec<u8>,
    },
    /// Phase 4g.d: pooled session is parked between ops. The
    /// underlying RFCOMM client + MasMceSession (now in
    /// MasState::Resting) are preserved so the next op can re-use
    /// the connection. Transitions into DrivingMas on `start_next_op`
    /// or AwaitingDisconnect-equivalent on `request_disconnect`.
    Resting {
        local_cid: u16,
        client: RfcommClientState,
        session: MasMceSession,
        obex_buffer: Vec<u8>,
    },
    /// Shared-mux equivalent of `Resting` — the MAS session is parked
    /// on a DLCI sitting on the inbound RFCOMM multiplexer.
    RestingShared {
        local_cid: u16,
        target_dlci: u8,
        session: MasMceSession,
        obex_buffer: Vec<u8>,
    },
    Done,
    Failed(String),
}

pub struct MapRuntime {
    connection_handle: u16,
    operation: Operation,
    mas_instance_id: u8,
    phase: MapRuntimePhase,
    pending_events: Vec<MapRuntimeEvent>,
    inbound_buffer: Arc<StdMutex<Vec<(u16, Vec<u8>)>>>,
    sdp_transaction_id: u16,
    /// Phase 4g.d: when true, OBEX session lives past op completion
    /// (ends in `MapRuntimePhase::Resting`) so the orchestrator can
    /// queue another op without paying SDP+RFCOMM+OBEX-CONNECT again.
    /// Default false to preserve the per-op behaviour the existing
    /// tests assume.
    pooled: bool,
}

impl MapRuntime {
    pub fn new(connection_handle: u16, operation: Operation, mas_instance_id: u8) -> Self {
        Self {
            connection_handle,
            operation,
            mas_instance_id,
            phase: MapRuntimePhase::Idle,
            pending_events: Vec::new(),
            inbound_buffer: Arc::new(StdMutex::new(Vec::new())),
            sdp_transaction_id: 0x0001,
            pooled: false,
        }
    }

    /// Phase 4g.d: opt this runtime into pooled mode. Must be called
    /// before `start()`. After each op completes the runtime parks in
    /// `MapRuntimePhase::Resting` and the orchestrator can either feed
    /// another op via `start_next_op` or tear down via
    /// `request_disconnect`.
    pub fn enable_pooling(&mut self) {
        self.pooled = true;
    }

    pub fn is_done(&self) -> bool {
        matches!(self.phase, MapRuntimePhase::Done)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.phase, MapRuntimePhase::Failed(_))
    }

    /// Phase 4g.d: true once an op has completed and the session is
    /// parked, ready to accept another via `start_next_op` (or
    /// disconnect via `request_disconnect`).
    pub fn is_resting(&self) -> bool {
        matches!(
            self.phase,
            MapRuntimePhase::Resting { .. } | MapRuntimePhase::RestingShared { .. }
        )
    }

    /// Phase 4g.d: feed a follow-up op into a resting runtime. Returns
    /// the ACL packets to write to the transport. On a non-resting
    /// runtime this errors — the orchestrator should always check
    /// `is_resting()` first.
    pub fn start_next_op(
        &mut self,
        operation: Operation,
        l2cap_state: &mut L2capState,
    ) -> Result<Vec<Vec<u8>>, String> {
        let phase = std::mem::replace(&mut self.phase, MapRuntimePhase::Done);
        match phase {
            MapRuntimePhase::Resting {
                local_cid,
                client,
                mut session,
                obex_buffer,
            } => {
                let next_req = match session.start_next_op(operation) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        self.phase = MapRuntimePhase::Resting {
                            local_cid,
                            client,
                            session,
                            obex_buffer,
                        };
                        return Err(e);
                    }
                };
                let uih = client.build_outbound_uih(&next_req)?;
                let acl = l2cap_state.send_on_channel(local_cid, &uih)?;
                self.phase = MapRuntimePhase::DrivingMas {
                    local_cid,
                    client,
                    session,
                    obex_buffer,
                };
                Ok(vec![acl])
            }
            MapRuntimePhase::RestingShared {
                local_cid,
                target_dlci,
                mut session,
                obex_buffer,
            } => {
                let next_req = match session.start_next_op(operation) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        self.phase = MapRuntimePhase::RestingShared {
                            local_cid,
                            target_dlci,
                            session,
                            obex_buffer,
                        };
                        return Err(e);
                    }
                };
                let acl = l2cap_state.rfcomm_send_uih_on_client_dlci(
                    local_cid,
                    target_dlci,
                    &next_req,
                )?;
                self.phase = MapRuntimePhase::DrivingMasShared {
                    local_cid,
                    target_dlci,
                    session,
                    obex_buffer,
                };
                Ok(vec![acl])
            }
            other => {
                let phase_dbg = format!("{:?}", other);
                self.phase = other;
                Err(format!(
                    "start_next_op called in non-Resting phase: {}",
                    phase_dbg
                ))
            }
        }
    }

    /// Phase 4g.d: tear down a resting runtime. Sends OBEX
    /// DISCONNECT; on response the runtime walks to Done and the
    /// orchestrator can drop it.
    pub fn request_disconnect(
        &mut self,
        l2cap_state: &mut L2capState,
    ) -> Result<Vec<Vec<u8>>, String> {
        let phase = std::mem::replace(&mut self.phase, MapRuntimePhase::Done);
        match phase {
            MapRuntimePhase::Resting {
                local_cid,
                client,
                mut session,
                obex_buffer,
            } => {
                let disc = session.request_disconnect()?;
                let uih = client.build_outbound_uih(&disc)?;
                let acl = l2cap_state.send_on_channel(local_cid, &uih)?;
                self.phase = MapRuntimePhase::DrivingMas {
                    local_cid,
                    client,
                    session,
                    obex_buffer,
                };
                Ok(vec![acl])
            }
            MapRuntimePhase::RestingShared {
                local_cid,
                target_dlci,
                mut session,
                obex_buffer,
            } => {
                let disc = session.request_disconnect()?;
                let acl =
                    l2cap_state.rfcomm_send_uih_on_client_dlci(local_cid, target_dlci, &disc)?;
                self.phase = MapRuntimePhase::DrivingMasShared {
                    local_cid,
                    target_dlci,
                    session,
                    obex_buffer,
                };
                Ok(vec![acl])
            }
            other => {
                let phase_dbg = format!("{:?}", other);
                self.phase = other;
                Err(format!(
                    "request_disconnect called in non-Resting phase: {}",
                    phase_dbg
                ))
            }
        }
    }

    pub fn take_events(&mut self) -> Vec<MapRuntimeEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Open the SDP outbound channel. Returns the ACL ConnectionRequest
    /// for the runtime caller to write to the transport.
    pub fn start(&mut self, l2cap_state: &mut L2capState) -> Result<Vec<Vec<u8>>, String> {
        if !matches!(self.phase, MapRuntimePhase::Idle) {
            return Err(format!(
                "MAP start called in non-Idle phase: {:?}",
                self.phase
            ));
        }
        let (local_cid, packet) = l2cap_state.open_outbound_channel_with_handler(
            self.connection_handle,
            PSM_SDP,
            Some(self.make_handler()),
        );
        self.phase = MapRuntimePhase::AwaitingSdpOpen { local_cid };
        Ok(vec![packet])
    }

    pub fn tick(&mut self, l2cap_state: &mut L2capState) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        let inbound = std::mem::take(
            &mut *self
                .inbound_buffer
                .lock()
                .map_err(|_| "MAP runtime inbound buffer poisoned".to_string())?,
        );
        for (cid, bytes) in inbound {
            self.consume_inbound(cid, bytes, l2cap_state, &mut out)?;
        }
        // Shared-mux path: drain client-DLCI events surfaced by
        // RfcommState on the inbound RFCOMM channel. These carry the
        // Open transition (start OBEX) and inbound UIH payloads
        // (advance OBEX state machine). Filter by our target DLCI so
        // we don't eat events that belong to a sibling runtime (PBAP)
        // riding the same shared mux — the unfiltered drain would
        // silently consume PBAP's Opened and stall its OBEX forever.
        let shared = match &self.phase {
            MapRuntimePhase::AwaitingSharedDlciOpen {
                local_cid,
                target_dlci,
            }
            | MapRuntimePhase::DrivingMasShared {
                local_cid,
                target_dlci,
                ..
            }
            | MapRuntimePhase::RestingShared {
                local_cid,
                target_dlci,
                ..
            } => Some((*local_cid, *target_dlci)),
            _ => None,
        };
        if let Some((cid, dlci)) = shared {
            for ev in l2cap_state.take_rfcomm_client_events_for_dlci(cid, dlci) {
                self.consume_shared_event(ev, l2cap_state, &mut out)?;
            }
        }
        self.drive_outbound(l2cap_state, &mut out)?;
        Ok(out)
    }

    fn make_handler(&self) -> PsmHandler {
        let buf = self.inbound_buffer.clone();
        Arc::new(move |channel, payload| {
            buf.lock()
                .map_err(|_| "MAP inbound buffer poisoned".to_string())?
                .push((channel.local_cid, payload.to_vec()));
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
        let current_phase = std::mem::replace(&mut self.phase, MapRuntimePhase::Done);
        match current_phase {
            MapRuntimePhase::AwaitingSdpResponse {
                local_cid,
                mut buffer,
            } if local_cid == cid => {
                buffer.extend_from_slice(&bytes);
                match parse_service_search_attribute_response(&buffer) {
                    Ok(resp) if resp.continuation_state.is_empty() => {
                        let server_channel = match self
                            .extract_mas_channel_from_attribute_lists(resp.attribute_lists)
                        {
                            Ok(c) => c,
                            Err(e) => {
                                self.fail(
                                    format!("MAP SDP: {}", e),
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
                        // Prefer to ride on the existing inbound RFCOMM
                        // multiplexer if one is up — Pixel/Bluedroid
                        // silently drops a second L2CAP/PSM 0x0003
                        // session per peer (see project memory
                        // "Bluedroid: one RFCOMM session per peer").
                        // Fall back to opening our own L2CAP+RFCOMM
                        // session only when no shared mux exists, so
                        // the unit-test harness (which doesn't simulate
                        // the inbound HFP path) keeps exercising the
                        // legacy state machine.
                        if let Some(shared_cid) =
                            l2cap_state.find_open_rfcomm_channel(self.connection_handle)
                        {
                            match l2cap_state.rfcomm_attach_client_dlci(shared_cid, server_channel)
                            {
                                Ok((target_dlci, pn_acl)) => {
                                    out.push(pn_acl);
                                    println!(
                                        "[AokieRadio] MAP SDP done — MAS server_channel={} → target dlci={} on shared mux cid 0x{:04x}",
                                        server_channel, target_dlci, shared_cid
                                    );
                                    self.phase = MapRuntimePhase::AwaitingSharedDlciOpen {
                                        local_cid: shared_cid,
                                        target_dlci,
                                    };
                                    return Ok(());
                                }
                                Err(e) => {
                                    self.fail(
                                        format!("MAP shared DLCI attach: {}", e),
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
                        println!(
                            "[AokieRadio] MAP SDP done — MAS RFCOMM channel {} on cid 0x{:04x} (no shared mux)",
                            server_channel, rfcomm_cid
                        );
                        self.phase = MapRuntimePhase::AwaitingRfcommOpen {
                            server_channel,
                            local_cid: rfcomm_cid,
                        };
                        Ok(())
                    }
                    Ok(_) => {
                        self.fail(
                            "MAP SDP: continuation state present (unexpected for our small query)"
                                .to_string(),
                            Some(local_cid),
                            l2cap_state,
                            out,
                        );
                        Ok(())
                    }
                    Err(_) => {
                        self.phase = MapRuntimePhase::AwaitingSdpResponse { local_cid, buffer };
                        Ok(())
                    }
                }
            }
            MapRuntimePhase::DrivingRfcomm {
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
                            println!(
                                "[AokieRadio] MAP RFCOMM opened (cid 0x{:04x}) — sending OBEX CONNECT",
                                local_cid
                            );
                            let mut tmp_session =
                                MasMceSession::new(self.operation.clone(), self.mas_instance_id);
                            // Phase 4g.d: propagate pooled flag so the
                            // session parks in Resting after each op
                            // instead of immediately disconnecting.
                            tmp_session.set_keep_alive(self.pooled);
                            if let Some(connect_req) = tmp_session.next_request() {
                                let uih = client.build_outbound_uih(&connect_req)?;
                                out.push(l2cap_state.send_on_channel(local_cid, &uih)?);
                                self.phase = MapRuntimePhase::DrivingMas {
                                    local_cid,
                                    client,
                                    session: tmp_session,
                                    obex_buffer: Vec::new(),
                                };
                                return Ok(());
                            } else {
                                self.fail(
                                    "MAS session refused to produce CONNECT request".to_string(),
                                    Some(local_cid),
                                    l2cap_state,
                                    out,
                                );
                                return Ok(());
                            }
                        }
                        RfcommClientEvent::Closed => {
                            println!("[AokieRadio] MAP RFCOMM closed before session opened");
                            self.fail(
                                "RFCOMM closed before MAP session opened".to_string(),
                                Some(local_cid),
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                        RfcommClientEvent::Failed(reason) => {
                            println!("[AokieRadio] MAP RFCOMM client failed: {}", reason);
                            self.fail(
                                format!("RFCOMM client failed: {}", reason),
                                Some(local_cid),
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                        RfcommClientEvent::Payload(_) => {}
                    }
                }
                self.phase = MapRuntimePhase::DrivingRfcomm { local_cid, client };
                Ok(())
            }
            MapRuntimePhase::DrivingMas {
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
                                            format!("MAP OBEX parse failed: {}", e),
                                            Some(local_cid),
                                            l2cap_state,
                                            out,
                                        );
                                        return Ok(());
                                    }
                                };
                                println!(
                                    "[AokieRadio] MAP OBEX response opcode 0x{:02x} (state before: {:?})",
                                    parsed.opcode, session.state()
                                );
                                if let Some(next_req) = session.handle_response(&parsed) {
                                    let uih = client.build_outbound_uih(&next_req)?;
                                    out.push(l2cap_state.send_on_channel(local_cid, &uih)?);
                                    println!(
                                        "[AokieRadio] MAP MAS sent next request — new state {:?}",
                                        session.state()
                                    );
                                }
                                match session.state() {
                                    MasState::Done => {
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
                                        let output = session.into_output();
                                        self.pending_events
                                            .push(MapRuntimeEvent::OperationCompleted(output));
                                        self.phase = MapRuntimePhase::Done;
                                        return Ok(());
                                    }
                                    MasState::Resting => {
                                        // Phase 4g.d: pooled session
                                        // finished an op cleanly. Emit
                                        // the OperationCompleted event
                                        // (so the orchestrator can
                                        // decide to queue another or
                                        // tear down) but keep the
                                        // RFCOMM/L2CAP connection up.
                                        let output = session.output().clone();
                                        self.pending_events
                                            .push(MapRuntimeEvent::OperationCompleted(output));
                                        self.phase = MapRuntimePhase::Resting {
                                            local_cid,
                                            client,
                                            session,
                                            obex_buffer,
                                        };
                                        return Ok(());
                                    }
                                    MasState::Failed(reason) => {
                                        let reason = reason.clone();
                                        self.fail(
                                            format!("MAS session failed: {}", reason),
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
                                "RFCOMM closed mid-MAP session".to_string(),
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
                        RfcommClientEvent::Opened => {}
                    }
                }
                self.phase = MapRuntimePhase::DrivingMas {
                    local_cid,
                    client,
                    session,
                    obex_buffer,
                };
                Ok(())
            }
            other => {
                self.phase = other;
                Ok(())
            }
        }
    }

    /// Process a single client-DLCI event from the shared inbound
    /// RFCOMM multiplexer. Mirrors the relevant branches of
    /// `consume_inbound` for `DrivingMas`, but drives the
    /// `*Shared` phase variants so we don't depend on owning an
    /// `RfcommClientState`.
    fn consume_shared_event(
        &mut self,
        ev: ClientDlciEvent,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), String> {
        match ev {
            ClientDlciEvent::Opened { dlci } => {
                let phase = std::mem::replace(&mut self.phase, MapRuntimePhase::Done);
                let MapRuntimePhase::AwaitingSharedDlciOpen {
                    local_cid,
                    target_dlci,
                } = phase
                else {
                    // Stale event for a non-shared phase — restore phase.
                    self.phase = phase;
                    return Ok(());
                };
                if dlci != target_dlci {
                    self.phase = MapRuntimePhase::AwaitingSharedDlciOpen {
                        local_cid,
                        target_dlci,
                    };
                    return Ok(());
                }
                let mut session = MasMceSession::new(self.operation.clone(), self.mas_instance_id);
                session.set_keep_alive(self.pooled);
                let Some(connect_req) = session.next_request() else {
                    self.fail(
                        "MAS session refused to produce CONNECT request".to_string(),
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
                println!(
                    "[AokieRadio] MAP shared DLCI {} OPEN — sending OBEX CONNECT",
                    target_dlci
                );
                self.phase = MapRuntimePhase::DrivingMasShared {
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
                let phase = std::mem::replace(&mut self.phase, MapRuntimePhase::Done);
                let MapRuntimePhase::DrivingMasShared {
                    local_cid,
                    target_dlci,
                    mut session,
                    mut obex_buffer,
                } = phase
                else {
                    // Payload arrived while not in DrivingMasShared —
                    // can happen briefly if `Opened` and `Payload` come
                    // back-to-back and we haven't run drive_outbound
                    // for the CONNECT yet. Push back into buffer of
                    // whatever phase we just clobbered if it had one.
                    self.phase = phase;
                    return Ok(());
                };
                if dlci != target_dlci {
                    self.phase = MapRuntimePhase::DrivingMasShared {
                        local_cid,
                        target_dlci,
                        session,
                        obex_buffer,
                    };
                    return Ok(());
                }
                obex_buffer.extend_from_slice(&payload);
                loop {
                    let take = match try_take_obex_packet(&obex_buffer) {
                        Ok(Some(n)) => n,
                        Ok(None) => break,
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
                                format!("MAP OBEX parse failed: {}", e),
                                None,
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                    };
                    println!(
                        "[AokieRadio] MAP OBEX response opcode 0x{:02x} (state before: {:?})",
                        parsed.opcode,
                        session.state()
                    );
                    if let Some(next_req) = session.handle_response(&parsed) {
                        let acl = l2cap_state.rfcomm_send_uih_on_client_dlci(
                            local_cid,
                            target_dlci,
                            &next_req,
                        )?;
                        out.push(acl);
                        println!(
                            "[AokieRadio] MAP MAS sent next request — new state {:?}",
                            session.state()
                        );
                    }
                    match session.state() {
                        MasState::Done => {
                            // Cleanly tear the DLCI down. We don't
                            // disconnect the L2CAP channel — it's the
                            // shared inbound HFP one.
                            if let Ok(disc_acl) =
                                l2cap_state.rfcomm_disc_client_dlci(local_cid, target_dlci)
                            {
                                out.push(disc_acl);
                            }
                            let output = session.into_output();
                            self.pending_events
                                .push(MapRuntimeEvent::OperationCompleted(output));
                            self.phase = MapRuntimePhase::Done;
                            return Ok(());
                        }
                        MasState::Resting => {
                            let output = session.output().clone();
                            self.pending_events
                                .push(MapRuntimeEvent::OperationCompleted(output));
                            self.phase = MapRuntimePhase::RestingShared {
                                local_cid,
                                target_dlci,
                                session,
                                obex_buffer,
                            };
                            return Ok(());
                        }
                        MasState::Failed(reason) => {
                            let reason = reason.clone();
                            self.fail(
                                format!("MAS session failed: {}", reason),
                                None,
                                l2cap_state,
                                out,
                            );
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                self.phase = MapRuntimePhase::DrivingMasShared {
                    local_cid,
                    target_dlci,
                    session,
                    obex_buffer,
                };
                Ok(())
            }
        }
    }

    fn drive_outbound(
        &mut self,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), String> {
        match &self.phase {
            MapRuntimePhase::AwaitingSdpOpen { local_cid } => {
                let cid = *local_cid;
                if let Some(channel) = l2cap_state.channel(cid) {
                    if channel.state == ChannelState::Open {
                        let req = build_service_search_attribute_request(
                            self.sdp_transaction_id,
                            UUID_MAP_MAS,
                            0xffff,
                        );
                        let acl = l2cap_state.send_on_channel(cid, &req)?;
                        out.push(acl);
                        self.phase = MapRuntimePhase::AwaitingSdpResponse {
                            local_cid: cid,
                            buffer: Vec::new(),
                        };
                    }
                }
            }
            MapRuntimePhase::AwaitingRfcommOpen {
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
                        println!(
                            "[AokieRadio] MAP RFCOMM L2CAP open (cid 0x{:04x}) — sent multiplexer SABM for channel {}",
                            cid, channel_num
                        );
                        self.phase = MapRuntimePhase::DrivingRfcomm {
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

    fn extract_mas_channel_from_attribute_lists(
        &self,
        attribute_lists: &[u8],
    ) -> Result<u8, String> {
        let records = iter_records(attribute_lists)?;
        for record in records {
            if let Some(channel) = extract_rfcomm_channel(record)? {
                return Ok(channel);
            }
        }
        Err("MAP MAS service record did not advertise an RFCOMM channel".to_string())
    }

    fn fail(
        &mut self,
        reason: String,
        active_cid: Option<u16>,
        l2cap_state: &mut L2capState,
        out: &mut Vec<Vec<u8>>,
    ) {
        if let Some(cid) = active_cid {
            if let Some(packet) = l2cap_state.disconnect_channel(cid) {
                out.push(packet);
            }
        }
        self.pending_events
            .push(MapRuntimeEvent::Failed(reason.clone()));
        self.phase = MapRuntimePhase::Failed(reason);
    }
}

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

fn expected_fixed_payload(state: &MasState) -> FixedPayload {
    match state {
        MasState::AwaitingConnect => FixedPayload::Connect,
        _ => FixedPayload::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aokie_radio::map_mas::Folder;

    #[test]
    fn map_runtime_start_opens_outbound_sdp_channel() {
        let mut runtime = MapRuntime::new(
            0x002a,
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 16,
                filter_message_type: 0xfc,
            },
            0,
        );
        let mut l2cap = L2capState::new();
        let acls = runtime.start(&mut l2cap).unwrap();
        assert_eq!(acls.len(), 1, "one ConnectionRequest");
        assert_eq!(l2cap.channel_count(), 1);
        // Calling start twice errors.
        assert!(runtime.start(&mut l2cap).is_err());
    }

    #[test]
    fn map_runtime_fails_cleanly_when_sdp_response_lacks_rfcomm_channel() {
        use crate::aokie_radio::l2cap::{
            build_acl_packet, build_basic_frame, ACL_BROADCAST_POINT_TO_POINT,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE, CID_SIGNALING,
        };
        let mut runtime = MapRuntime::new(
            0x002a,
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 16,
                filter_message_type: 0xfc,
            },
            0,
        );
        let mut l2cap = L2capState::new();
        let _ = runtime.start(&mut l2cap).unwrap();

        // Walk the SDP L2CAP channel to Open.
        let local_cid = 0x0040u16;
        let remote_cid = 0x0070u16;
        let cid_bytes = local_cid.to_le_bytes();
        let rcid_bytes = remote_cid.to_le_bytes();
        let conn_resp = build_acl_packet(
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x03,
                    0x80,
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
            0x002a,
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
            0x002a,
            ACL_PACKET_BOUNDARY_FIRST_NON_FLUSHABLE,
            ACL_BROADCAST_POINT_TO_POINT,
            &build_basic_frame(
                CID_SIGNALING,
                &[
                    0x05,
                    0x81,
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
        // Tick to send the SDP request.
        let _ = runtime.tick(&mut l2cap).unwrap();

        // SDP response with one record but no ProtocolDescriptorList.
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
        let basic = build_basic_frame(local_cid, &pdu);
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
            events.iter().any(|e| matches!(
                e,
                MapRuntimeEvent::Failed(reason) if reason.contains("did not advertise")
            )),
            "expected Failed event, got {:?}",
            events
        );
        assert!(runtime.is_failed());
    }

    #[test]
    fn pooled_runtime_apis_reject_non_resting_state() {
        // start_next_op and request_disconnect both require the
        // runtime to be parked in Resting. Calling them on a
        // freshly-constructed (Idle) runtime must error rather than
        // panic or corrupt state.
        let mut runtime = MapRuntime::new(
            0x002a,
            Operation::SetNotificationRegistration { enabled: true },
            0,
        );
        runtime.enable_pooling();
        let mut l2cap = L2capState::new();
        let next_err = runtime.start_next_op(
            Operation::SetNotificationRegistration { enabled: false },
            &mut l2cap,
        );
        assert!(next_err.is_err());
        let disc_err = runtime.request_disconnect(&mut l2cap);
        assert!(disc_err.is_err());
        assert!(!runtime.is_resting());
        assert!(!runtime.is_done());
        assert!(!runtime.is_failed());
    }
}
