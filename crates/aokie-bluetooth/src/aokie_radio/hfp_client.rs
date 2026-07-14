//! Initiator-side HFP state — the missing half of HARD-001's outbound
//! reconnect. When WE page a bonded phone, Bluedroid does not initiate
//! any profile setup (two live test runs, see `runtime.rs`
//! AUTO_RECONNECT_OUTBOUND_PAGE), so a headset-style reconnect must
//! open the RFCOMM multiplexer itself and drive the HFP Service Level
//! Connection exactly like a real hands-free unit does.
//!
//! `HfpClientState` = `RfcommClientState` (the live-proven initiator
//! multiplexer machine from the MAP/PBAP work — its C/R bit choices
//! are the spec-correct initiator set) + the same `hfp::` AT command
//! queue/pump the inbound server path runs in `RfcommState`. It is
//! deliberately a SEPARATE struct rather than a role flag on
//! `RfcommState`: the server path's frame C/R bits are empirically
//! tuned against live phones and must not grow conditionals.
//!
//! Ownership mirrors the server path: the struct lives on the outbound
//! `L2capChannel` (`hfp_client` slot), fed by the per-channel upper
//! handler, and surfaces the SAME `hfp::HfpEvent` stream through
//! `L2capState::take_hfp_events` — so everything downstream (call
//! session tracking, SCO, auto-answer, keepalive AT+CIND?) works
//! identically whichever side initiated the connection.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::hfp;
use super::rfcomm::{ClientDlciEvent, RfcommClientEvent, RfcommClientState};

pub struct HfpClientState {
    client: RfcommClientState,
    hfp_state: hfp::HfpHandsFreeState,
    /// Incomplete trailing AT line carried across UIH frames (the live
    /// 2026-07-14 phantom-answer incident hit THIS path: the +CIND=?
    /// definitions line fragmented on the initiator mux).
    line_carry: String,
    pending_commands: VecDeque<hfp::HfpAtCommand>,
    in_flight_command: Option<hfp::HfpAtCommand>,
    in_flight_sent_at: Option<Instant>,
    slc_failed: bool,
    events: Vec<hfp::HfpEvent>,
    wbs_supported: bool,
    /// Latched once the peer closes the channel (DISC / DM / mux
    /// failure) so call-control sends stop producing frames.
    closed: bool,
}

impl HfpClientState {
    pub fn new(server_channel: u8, wbs_supported: bool) -> Self {
        Self {
            client: RfcommClientState::new(server_channel),
            hfp_state: hfp::HfpHandsFreeState::new(),
            line_carry: String::new(),
            pending_commands: VecDeque::new(),
            in_flight_command: None,
            in_flight_sent_at: None,
            slc_failed: false,
            events: Vec::new(),
            wbs_supported,
            closed: false,
        }
    }

    /// Originate the multiplexer SABM. Call once, after the outbound
    /// L2CAP channel to PSM_RFCOMM transitions to `Open`. The returned
    /// RFCOMM frame still needs L2CAP/ACL wrapping (`send_on_channel`).
    pub fn kickoff(&mut self) -> Result<Vec<u8>, String> {
        // Arm the stall clock on the handshake itself, not just the AT
        // queue: a peer that UAs the mux but never answers our PN/SABM
        // would otherwise hang silently forever (the AT stall watchdog
        // only starts once the first AT command goes out).
        self.in_flight_sent_at = Some(Instant::now());
        self.client.kickoff()
    }

    pub fn is_open(&self) -> bool {
        self.client.is_open() && !self.closed
    }

    /// True while this session's multiplexer is up — the shared-mux
    /// discovery hook for MAP/PBAP. Bluedroid allows ONE RFCOMM session
    /// per peer, so on an outbound-connected phone the OBEX profiles
    /// MUST ride this mux instead of opening their own (a second
    /// PSM-0x0003 session is config-acked and then never SABM-answered;
    /// live 2026-07-13 that stall ate the first kickoff SMS AND pulled
    /// the phone's HFP traffic onto the dead channel).
    pub fn mux_is_open(&self) -> bool {
        self.client.mux_is_open() && !self.closed
    }

    /// Attach an outbound DLCI for another profile (MAP MAS, PBAP PSE)
    /// on this session's multiplexer. Delegates to the underlying
    /// initiator-mux machinery; see `RfcommClientState::attach_client_dlci`.
    pub fn attach_client_dlci(&mut self, server_channel: u8) -> Result<(u8, Vec<u8>), String> {
        if self.closed {
            return Err("attach_client_dlci: HFP client session is closed".to_string());
        }
        self.client.attach_client_dlci(server_channel)
    }

    /// Drain secondary-DLCI events for `dlci` (same contract as
    /// `RfcommState::take_client_events_for_dlci`).
    pub fn take_client_events_for_dlci(&mut self, dlci: u8) -> Vec<ClientDlciEvent> {
        self.client.take_client_events_for_dlci(dlci)
    }

    /// Build an OBEX-bearing UIH on a secondary DLCI.
    pub fn build_uih_on_client_dlci(&mut self, dlci: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        self.client.build_uih_on_client_dlci(dlci, payload)
    }

    /// Force-DISC a secondary DLCI (MAS-stall recovery). Never touches
    /// the primary HFP DLCI.
    pub fn build_force_disc_secondary_dlci(&mut self, dlci: u8) -> Vec<u8> {
        self.client.build_force_disc_secondary_dlci(dlci)
    }

    pub fn service_level_ready(&self) -> bool {
        self.hfp_state.service_level_ready()
    }

    pub fn hfp_state(&self) -> &hfp::HfpHandsFreeState {
        &self.hfp_state
    }

    pub fn take_hfp_events(&mut self) -> Vec<hfp::HfpEvent> {
        std::mem::take(&mut self.events)
    }

    /// Drive the state machine with an inbound RFCOMM frame. Returns
    /// zero or more RFCOMM frames to send back (caller wraps in
    /// L2CAP/ACL). Mirrors `RfcommState::handle_packet` +
    /// `handle_hfp_payload` for the initiator role.
    pub fn handle_packet(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let mut out = self.client.handle_packet(packet)?;
        for event in self.client.take_events() {
            match event {
                RfcommClientEvent::Opened => {
                    // MSC settled in both directions (RfcommClientState
                    // only emits Opened then) — same precondition the
                    // server path waits for before kicking AT (HFP
                    // §4.2.1). Queue the SLC and fire AT+BRSF.
                    eprintln!(
                        "[AokieRadio] HFP client DLCI open — kicking outbound SLC sequence"
                    );
                    self.pending_commands = hfp::HfpHandsFreeState::initial_service_level_commands(
                        self.wbs_supported,
                    )
                    .into();
                    if let Some(frame) = self.next_command_frame()? {
                        out.push(frame);
                    }
                }
                RfcommClientEvent::Payload(payload) => {
                    out.extend(self.handle_ag_payload(&payload)?);
                }
                RfcommClientEvent::Closed => {
                    self.closed = true;
                    self.in_flight_command = None;
                    self.in_flight_sent_at = None;
                    if !self.hfp_state.service_level_ready() {
                        self.slc_failed = true;
                        self.events
                            .push(hfp::HfpEvent::ServiceLevelConnectionFailed(
                                "peer closed the RFCOMM channel during outbound SLC".to_string(),
                            ));
                    } else {
                        eprintln!(
                            "[AokieRadio] HFP client channel closed by peer (post-SLC)"
                        );
                    }
                }
                RfcommClientEvent::Failed(reason) => {
                    self.closed = true;
                    self.slc_failed = true;
                    self.in_flight_command = None;
                    self.in_flight_sent_at = None;
                    self.events
                        .push(hfp::HfpEvent::ServiceLevelConnectionFailed(format!(
                            "outbound RFCOMM handshake failed: {}",
                            reason
                        )));
                }
            }
        }
        // Credit-arrival pump: the frame we just processed may have been
        // the peer's credit grant — if a queued command was held back by
        // `can_send_data`, send it now. No-op when something is already
        // in flight or the queue is empty.
        if self.is_open() && !self.slc_failed && self.in_flight_command.is_none() {
            if let Some(frame) = self.next_command_frame()? {
                out.push(frame);
            }
        }
        Ok(out)
    }

    /// Build a call-control (or keepalive) AT command frame, exactly
    /// like `RfcommState::build_call_control_command` — gated on the
    /// channel being open.
    pub fn build_call_control_command(
        &mut self,
        command: hfp::HfpAtCommand,
    ) -> Option<Vec<u8>> {
        if !self.is_open() {
            return None;
        }
        self.build_at_frame(command).ok()
    }

    /// SLC stall watchdog — same contract as `RfcommState::tick_hfp_stall`.
    pub fn tick_hfp_stall(&mut self, now: Instant, timeout: Duration) -> bool {
        let Some(sent_at) = self.in_flight_sent_at else {
            return false;
        };
        if now.saturating_duration_since(sent_at) < timeout {
            return false;
        }
        let stalled = self
            .in_flight_command
            .take()
            .map(|c| format!("{:?}", c))
            .unwrap_or_else(|| "RFCOMM handshake".to_string());
        self.in_flight_sent_at = None;
        if !self.hfp_state.service_level_ready() {
            self.pending_commands.clear();
            self.slc_failed = true;
        }
        self.events
            .push(hfp::HfpEvent::ServiceLevelConnectionFailed(format!(
                "outbound {} timed out after {:?}",
                stalled, timeout
            )));
        true
    }

    fn handle_ag_payload(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let mut responses = Vec::new();
        for result in hfp::parse_ag_results_buffered(&mut self.line_carry, payload)? {
            self.events.extend(self.hfp_state.apply_result(&result));
            match result {
                hfp::HfpAgResult::Ok => {
                    self.in_flight_command = None;
                    self.in_flight_sent_at = None;
                    if !self.slc_failed {
                        if let Some(frame) = self.next_command_frame()? {
                            responses.push(frame);
                        } else if self.pending_commands.is_empty() {
                            // Lost/garbled +CIND=? definitions (phantom-answer
                            // incidents): re-request ONCE before readiness —
                            // ringing on default indices misreads as answered.
                            if self.hfp_state.needs_indicator_definitions_retry() {
                                self.pending_commands
                                    .push_back(hfp::HfpAtCommand::RetrieveIndicators);
                                self.pending_commands
                                    .push_back(hfp::HfpAtCommand::RetrieveIndicatorStatus);
                                if let Some(frame) = self.next_command_frame()? {
                                    responses.push(frame);
                                }
                            } else {
                                // Only an EMPTY queue means the SLC sequence
                                // finished — a command held back by credit
                                // flow control must not fake readiness.
                                if let Some(event) = self.hfp_state.mark_service_level_ready() {
                                    self.events.push(event);
                                }
                            }
                        }
                    }
                }
                hfp::HfpAgResult::Error => {
                    // Same policy as the inbound path: an ERROR during
                    // SLC is fatal (no safe blind retry); post-SLC
                    // ERRORs surface but don't unready the connection.
                    let failed = self.in_flight_command.take();
                    self.in_flight_sent_at = None;
                    if !self.hfp_state.service_level_ready() {
                        self.pending_commands.clear();
                        self.slc_failed = true;
                    }
                    let label = failed
                        .map(|c| format!("{:?}", c))
                        .unwrap_or_else(|| "unsolicited".to_string());
                    self.events
                        .push(hfp::HfpEvent::ServiceLevelConnectionFailed(label));
                }
                hfp::HfpAgResult::SelectedCodec(codec) => {
                    responses.push(self.build_at_frame(hfp::HfpAtCommand::ConfirmCodec(codec))?);
                }
                _ => {}
            }
        }
        Ok(responses)
    }

    fn next_command_frame(&mut self) -> Result<Option<Vec<u8>>, String> {
        // Credit-based flow control: hold the next AT command until the
        // peer has granted us a transmit credit (Bluedroid's PN response
        // says 0; the real grant lands as a credit UIH moments after the
        // channel opens). `handle_packet` re-runs this pump after every
        // inbound frame, so the held command goes out the instant the
        // grant arrives.
        if !self.client.can_send_data() {
            return Ok(None);
        }
        let Some(command) = self.pending_commands.pop_front() else {
            return Ok(None);
        };
        let frame = self.build_at_frame(command.clone())?;
        self.in_flight_command = Some(command);
        self.in_flight_sent_at = Some(Instant::now());
        Ok(Some(frame))
    }

    fn build_at_frame(&mut self, command: hfp::HfpAtCommand) -> Result<Vec<u8>, String> {
        self.hfp_state.mark_command_sent(&command);
        self.client
            .build_outbound_uih(&hfp::build_at_command(command))
    }
}

#[cfg(test)]
mod tests {
    use super::super::rfcomm::{
        build_modem_status_command, build_modem_status_response,
        build_parameter_negotiation_response, build_parameter_negotiation_response_cfc, build_ua,
        build_uih, parse_frame, server_channel_dlci, RfcommFrameKind, RFCOMM_DLCI_MULTIPLEXER,
    };
    use super::*;

    const AG_CHANNEL: u8 = 1; // Bluedroid advertises HFP AG on channel 1.

    fn target_dlci() -> u8 {
        server_channel_dlci(AG_CHANNEL, true)
    }

    /// Walk the RFCOMM client handshake as the AG would: UA the mux
    /// SABM, ack PN, UA the target SABM, exchange MSC both ways.
    /// Returns the frames our side produced from the final step (which
    /// should include AT+BRSF).
    fn open_channel(state: &mut HfpClientState) -> Vec<Vec<u8>> {
        let pn_rsp = build_parameter_negotiation_response(target_dlci(), 7, 127, 0);
        open_channel_with_pn(state, &pn_rsp)
    }

    fn open_channel_with_pn(state: &mut HfpClientState, pn_rsp: &[u8]) -> Vec<Vec<u8>> {
        let kick = state.kickoff().expect("kickoff");
        let frame = parse_frame(&kick).expect("kickoff frame");
        assert_eq!(frame.kind, RfcommFrameKind::Sabm);
        assert_eq!(frame.dlci, RFCOMM_DLCI_MULTIPLEXER);

        // UA(mux) → our PN command goes out.
        let out = state
            .handle_packet(&build_ua(RFCOMM_DLCI_MULTIPLEXER, false))
            .expect("ua mux");
        assert_eq!(out.len(), 1, "expected PN command after mux UA");
        // The PN command must grant the AG a NON-ZERO initial credit
        // budget — granting 0 left the live Bluedroid AG unable to send
        // +BRSF (HARD-001 outbound SLC: 5s of silence, then DISC).
        let pn_cmd = parse_frame(&out[0]).expect("pn frame");
        let initial_credits = pn_cmd.payload[9];
        assert!(
            initial_credits > 0,
            "PN command must grant initial credits, got {initial_credits}"
        );

        // PN response → our SABM(target).
        let out = state
            .handle_packet(&build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, pn_rsp))
            .expect("pn rsp");
        assert_eq!(out.len(), 1, "expected target SABM after PN response");
        let sabm = parse_frame(&out[0]).expect("sabm frame");
        assert_eq!(sabm.kind, RfcommFrameKind::Sabm);
        assert_eq!(sabm.dlci, target_dlci());

        // UA(target) → our MSC CMD.
        let out = state
            .handle_packet(&build_ua(target_dlci(), false))
            .expect("ua target");
        assert_eq!(out.len(), 1, "expected MSC CMD after target UA");

        // Peer MSC RSP (acks ours) + peer MSC CMD (we reply RSP).
        let msc_rsp = build_modem_status_response(target_dlci(), 0x8d);
        let out = state
            .handle_packet(&build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &msc_rsp))
            .expect("msc rsp");
        assert!(out.is_empty());
        let msc_cmd = build_modem_status_command(target_dlci(), 0x8d);
        state
            .handle_packet(&build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &msc_cmd))
            .expect("msc cmd")
    }

    fn ag_payload(state: &mut HfpClientState, text: &str) -> Vec<Vec<u8>> {
        state
            .handle_packet(&build_uih(target_dlci(), false, None, text.as_bytes()))
            .expect("ag payload")
    }

    fn frame_payload_string(frame_bytes: &[u8]) -> String {
        let frame = parse_frame(frame_bytes).expect("parse our frame");
        String::from_utf8_lossy(frame.payload).to_string()
    }

    #[test]
    fn opened_channel_kicks_slc_with_at_brsf() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        let out = open_channel(&mut state);
        // MSC RSP to the peer's CMD + the first SLC command.
        assert!(state.is_open());
        let at = out
            .iter()
            .map(|f| frame_payload_string(f))
            .find(|p| p.contains("AT+BRSF"));
        assert!(at.is_some(), "expected AT+BRSF after channel open: {out:?}");
    }

    /// The live HARD-001 outbound-SLC failure mode: Bluedroid accepts
    /// credit-based flow control with 0 initial credits in its PN
    /// response, then delivers the real grant as a zero-length credit
    /// UIH after the channel opens. AT+BRSF must WAIT for that grant —
    /// sending it at zero credits gets it silently discarded and the AG
    /// DISCs the channel 5s later.
    #[test]
    fn cfc_holds_at_brsf_until_the_credit_grant_arrives() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        let pn_rsp = build_parameter_negotiation_response_cfc(target_dlci(), 7, 127, 0);
        let out = open_channel_with_pn(&mut state, &pn_rsp);
        assert!(state.is_open());
        assert!(
            !out.iter()
                .map(|f| frame_payload_string(f))
                .any(|p| p.contains("AT+BRSF")),
            "AT+BRSF must be held at zero tx credits: {out:?}"
        );

        // The AG's credit grant (zero-length UIH with a credits byte).
        let grant = build_uih(target_dlci(), false, Some(7), &[]);
        let out = state.handle_packet(&grant).expect("credit grant");
        assert!(
            out.iter()
                .map(|f| frame_payload_string(f))
                .any(|p| p.contains("AT+BRSF")),
            "AT+BRSF must go out the moment credits arrive: {out:?}"
        );
    }

    /// Under CFC we must refill the AG's credits as its replies consume
    /// them — an AG that runs dry stops talking mid-SLC (the same stall
    /// we suffered, mirrored). Expect a zero-length credit top-up UIH
    /// among our outputs once the peer's budget runs low.
    #[test]
    fn cfc_tops_up_the_peers_credits_as_data_arrives() {
        let mut state = HfpClientState::new(AG_CHANNEL, true);
        let pn_rsp = build_parameter_negotiation_response_cfc(target_dlci(), 7, 127, 0);
        open_channel_with_pn(&mut state, &pn_rsp);
        state
            .handle_packet(&build_uih(target_dlci(), false, Some(7), &[]))
            .expect("credit grant");

        let mut topup_seen = false;
        for _ in 0..6 {
            let out = ag_payload(&mut state, "\r\nOK\r\n");
            for f in &out {
                let frame = parse_frame(f).expect("our frame");
                if frame.dlci == target_dlci()
                    && frame.payload.is_empty()
                    && frame.credits.unwrap_or(0) > 0
                {
                    topup_seen = true;
                }
            }
        }
        assert!(topup_seen, "expected a credit top-up UIH toward the AG");
    }

    #[test]
    fn slc_queue_drains_to_service_ready() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);

        // Reply to each queued command the way a healthy AG would.
        let replies = [
            "\r\n+BRSF: 871\r\n\r\nOK\r\n",
            "\r\nOK\r\n", // AT+BAC
            "\r\n+CIND: (\"call\",(0,1)),(\"callsetup\",(0-3))\r\n\r\nOK\r\n",
            "\r\n+CIND: 0,0\r\n\r\nOK\r\n",
            "\r\n+CHLD: (0,1,2,3)\r\n\r\nOK\r\n",
            "\r\nOK\r\n", // AT+CLIP
            "\r\nOK\r\n", // AT+CMER
            "\r\nOK\r\n", // AT+BIA
            "\r\nOK\r\n", // AT+NREC
        ];
        for reply in replies {
            ag_payload(&mut state, reply);
        }
        let events = state.take_hfp_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, hfp::HfpEvent::ServiceLevelConnectionReady)),
            "expected SLC ready after the queue drained: {events:?}"
        );
        assert!(state.service_level_ready());
    }

    #[test]
    fn error_during_slc_fails_the_connection() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);
        ag_payload(&mut state, "\r\nERROR\r\n");
        let events = state.take_hfp_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, hfp::HfpEvent::ServiceLevelConnectionFailed(_))),
            "ERROR during SLC must surface a failure: {events:?}"
        );
        assert!(!state.service_level_ready());
    }

    #[test]
    fn unsolicited_ring_after_ready_surfaces_call_events() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);
        for reply in [
            "\r\n+BRSF: 871\r\n\r\nOK\r\n",
            "\r\nOK\r\n",
            "\r\n+CIND: (\"call\",(0,1)),(\"callsetup\",(0-3))\r\n\r\nOK\r\n",
            "\r\n+CIND: 0,0\r\n\r\nOK\r\n",
            "\r\n+CHLD: (0,1,2,3)\r\n\r\nOK\r\n",
            "\r\nOK\r\n",
            "\r\nOK\r\n",
            "\r\nOK\r\n",
            "\r\nOK\r\n",
        ] {
            ag_payload(&mut state, reply);
        }
        state.take_hfp_events();
        ag_payload(&mut state, "\r\nRING\r\n");
        let events = state.take_hfp_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, hfp::HfpEvent::Ringing | hfp::HfpEvent::IncomingCall)),
            "RING should surface through the same event stream: {events:?}"
        );
        // Call control works over the client channel too.
        let ata = state.build_call_control_command(hfp::HfpAtCommand::Answer);
        assert!(ata.is_some(), "ATA should build once the channel is open");
        assert!(frame_payload_string(&ata.unwrap()).contains("ATA"));
    }

    #[test]
    fn stall_watchdog_fails_a_silent_ag() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);
        // AT+BRSF is in flight; the AG never replies.
        let fired = state.tick_hfp_stall(
            Instant::now() + Duration::from_secs(11),
            Duration::from_secs(10),
        );
        assert!(fired);
        let events = state.take_hfp_events();
        assert!(events
            .iter()
            .any(|e| matches!(e, hfp::HfpEvent::ServiceLevelConnectionFailed(_))));
    }

    #[test]
    fn selected_codec_is_confirmed_immediately() {
        let mut state = HfpClientState::new(AG_CHANNEL, true);
        open_channel(&mut state);
        let out = ag_payload(&mut state, "\r\n+BCS: 2\r\n");
        assert!(
            out.iter()
                .map(|f| frame_payload_string(f))
                .any(|p| p.contains("AT+BCS=2")),
            "+BCS must be answered with AT+BCS: {out:?}"
        );
    }

    // ── Shared initiator mux: MAP/PBAP secondary DLCIs (2026-07-13) ──
    // Bluedroid runs ONE RFCOMM session per peer, so on an outbound-
    // connected phone the OBEX profiles must ride the HFP client's mux.
    // These walk the exact attach → PN → SABM → MSC → Open → payload
    // sequence the live MAS runtime drives via the L2capState accessors.

    use super::super::rfcomm::{build_dm, ClientDlciEvent};

    const MAS_CHANNEL: u8 = 5; // Pixel advertises MAP MAS on channel 5.

    fn mas_dlci() -> u8 {
        server_channel_dlci(MAS_CHANNEL, true)
    }

    /// Walk a secondary DLCI to Open on an already-open HFP client mux.
    fn open_mas_dlci(state: &mut HfpClientState) -> u8 {
        let (dlci, pn_frame) = state.attach_client_dlci(MAS_CHANNEL).expect("attach");
        assert_eq!(dlci, mas_dlci());
        let pn = parse_frame(&pn_frame).expect("pn frame");
        assert_eq!(pn.kind, RfcommFrameKind::Uih);
        assert_eq!(pn.dlci, RFCOMM_DLCI_MULTIPLEXER);

        // PN response (CFC accepted, 0 initial credits — Bluedroid's
        // observed shape) → our SABM on the MAS DLCI.
        let pn_rsp = build_parameter_negotiation_response_cfc(dlci, 7, 127, 0);
        let out = state
            .handle_packet(&build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &pn_rsp))
            .expect("pn rsp");
        let sabm = out
            .iter()
            .map(|f| parse_frame(f).expect("frame"))
            .find(|f| f.kind == RfcommFrameKind::Sabm)
            .expect("SABM after PN response");
        assert_eq!(sabm.dlci, dlci);

        // UA(MAS) → our MSC CMD; peer MSC RSP + CMD → Opened.
        let out = state.handle_packet(&build_ua(dlci, false)).expect("ua");
        assert_eq!(out.len(), 1, "expected MSC CMD after MAS UA");
        let msc_rsp = build_modem_status_response(dlci, 0x8d);
        state
            .handle_packet(&build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &msc_rsp))
            .expect("msc rsp");
        let msc_cmd = build_modem_status_command(dlci, 0x8d);
        state
            .handle_packet(&build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &msc_cmd))
            .expect("msc cmd");
        let events = state.take_client_events_for_dlci(dlci);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ClientDlciEvent::Opened { .. })),
            "expected Opened for the MAS DLCI: {events:?}"
        );
        dlci
    }

    #[test]
    fn mas_dlci_opens_on_the_shared_initiator_mux_and_carries_obex() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);
        assert!(state.mux_is_open());
        let dlci = open_mas_dlci(&mut state);

        // Credit grant, then OBEX both ways.
        state
            .handle_packet(&build_uih(dlci, false, Some(7), &[]))
            .expect("credit grant");
        let uih = state
            .build_uih_on_client_dlci(dlci, b"obex-connect")
            .expect("obex uih");
        let f = parse_frame(&uih).expect("frame");
        assert_eq!(f.dlci, dlci);
        assert_eq!(f.kind, RfcommFrameKind::Uih);

        state
            .handle_packet(&build_uih(dlci, false, None, b"obex-response"))
            .expect("obex payload");
        let events = state.take_client_events_for_dlci(dlci);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ClientDlciEvent::Payload { .. })),
            "expected the OBEX payload event: {events:?}"
        );
        // HFP untouched throughout.
        assert!(state.is_open());
    }

    #[test]
    fn mas_refusal_fails_only_the_mas_dlci_never_the_hfp_session() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);
        let (dlci, _) = state.attach_client_dlci(MAS_CHANNEL).expect("attach");
        // Peer refuses with DM on the MAS DLCI.
        state
            .handle_packet(&build_dm(dlci, false))
            .expect("dm on mas");
        let events = state.take_client_events_for_dlci(dlci);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ClientDlciEvent::Failed { .. })),
            "expected Failed for the MAS DLCI: {events:?}"
        );
        // The phone link must survive an OBEX refusal.
        assert!(state.is_open(), "HFP must stay open after a MAS DM");
        assert!(state.mux_is_open());
        // And the DLCI slot must be reusable — a Failed tombstone would
        // wedge every later poll with 'dlci already in use' (live
        // 2026-07-13: the PollInbox retry loop died exactly this way).
        let (again, _) = state
            .attach_client_dlci(MAS_CHANNEL)
            .expect("re-attach after a peer DM");
        assert_eq!(again, dlci);
    }

    #[test]
    fn hfp_at_traffic_still_flows_while_a_mas_dlci_is_mid_handshake() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);
        let _ = state.attach_client_dlci(MAS_CHANNEL).expect("attach");
        // An unsolicited RING on the HFP DLCI mid-MAS-handshake must
        // still surface — this is the live 2026-07-13 regression shape
        // (the duplicate-session path swallowed HFP frames entirely).
        state.take_hfp_events();
        ag_payload(&mut state, "\r\nRING\r\n");
        let events = state.take_hfp_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, hfp::HfpEvent::Ringing | hfp::HfpEvent::IncomingCall)),
            "RING must reach the HFP pump during a MAS handshake: {events:?}"
        );
    }

    #[test]
    fn force_disc_frees_the_dlci_for_a_fresh_attach() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);
        let dlci = open_mas_dlci(&mut state);
        let disc = state.build_force_disc_secondary_dlci(dlci);
        let f = parse_frame(&disc).expect("disc frame");
        assert_eq!(f.kind, RfcommFrameKind::Disc);
        assert_eq!(f.dlci, dlci);
        // Tracking dropped → re-attach works (fresh PN).
        let (again, _) = state.attach_client_dlci(MAS_CHANNEL).expect("re-attach");
        assert_eq!(again, dlci);
    }

    #[test]
    fn stray_dm_or_disc_for_an_unknown_dlci_never_kills_the_session() {
        // Live 2026-07-13: stall recovery DISC'd the (never-open) MNS
        // dlci 4; the phone's DM answer hit the session-wide catch-all
        // and tore down a healthy HFP link mid-conversation.
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        open_channel(&mut state);
        state
            .handle_packet(&build_dm(4, false))
            .expect("stray dm");
        assert!(state.is_open(), "HFP must survive a DM for dlci 4");
        assert!(state.mux_is_open());
        let out = state
            .handle_packet(&super::super::rfcomm::build_disc(4, false))
            .expect("stray disc");
        assert!(state.is_open(), "HFP must survive a DISC for dlci 4");
        // The DISC gets the spec's DM answer, not a session teardown.
        let reply = parse_frame(&out[0]).expect("dm reply");
        assert_eq!(reply.kind, RfcommFrameKind::Dm);
        assert_eq!(reply.dlci, 4);
        // The primary DLCI still fails the session, as before.
        state
            .handle_packet(&build_dm(target_dlci(), false))
            .expect("dm on primary");
        assert!(!state.is_open(), "a DM on the HFP DLCI is still fatal");
    }

    #[test]
    fn attach_refuses_when_the_mux_is_not_open_or_the_dlci_is_taken() {
        let mut state = HfpClientState::new(AG_CHANNEL, false);
        assert!(state.attach_client_dlci(MAS_CHANNEL).is_err(), "no mux yet");
        open_channel(&mut state);
        let _ = state.attach_client_dlci(MAS_CHANNEL).expect("attach");
        assert!(
            state.attach_client_dlci(MAS_CHANNEL).is_err(),
            "dlci already in use"
        );
        assert!(
            state.attach_client_dlci(AG_CHANNEL).is_err(),
            "the primary HFP dlci is never attachable"
        );
    }
}
