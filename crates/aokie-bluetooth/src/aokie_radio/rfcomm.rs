use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{hfp, sdp::AOKIE_RFCOMM_CHANNEL};

/// Bundle of callbacks an RFCOMM server-channel tenant supplies. Phase
/// 0c of the MAP/PBAP plan: the HFP path stays special-cased inline (it
/// has bespoke MSC/AT/SLC state inside `RfcommState`), but additional
/// profiles (MAP MAS, MAP MNS, PBAP PSE) register their channel via
/// `RfcommState::register_server_channel`. SABM / UIH / DISC on a
/// registered DLCI fan out to these closures instead of being silently
/// dropped.
#[derive(Clone)]
pub struct ServerChannelHandlers {
    /// Called when the peer sends SABM on this server channel. Return
    /// the frames the layer should send back (typically a UA, possibly
    /// followed by profile-specific setup messages).
    pub on_sabm: Arc<dyn Fn() -> Vec<Vec<u8>> + Send + Sync>,
    /// Called when the peer sends DISC. Return the UA reply.
    pub on_disc: Arc<dyn Fn() -> Vec<u8> + Send + Sync>,
    /// Called when a UIH payload arrives on this channel. Return zero or
    /// more reply frames.
    pub on_uih: Arc<dyn Fn(&[u8]) -> Result<Vec<Vec<u8>>, String> + Send + Sync>,
}

pub const RFCOMM_DLCI_MULTIPLEXER: u8 = 0;
pub const RFCOMM_CONTROL_SABM: u8 = 0x3f;
pub const RFCOMM_CONTROL_UA: u8 = 0x73;
pub const RFCOMM_CONTROL_DM: u8 = 0x0f;
pub const RFCOMM_CONTROL_DM_PF: u8 = 0x1f;
pub const RFCOMM_CONTROL_DISC: u8 = 0x53;
pub const RFCOMM_CONTROL_UIH: u8 = 0xef;
pub const RFCOMM_CONTROL_UIH_PF: u8 = 0xff;

pub const RFCOMM_MUX_FCON_CMD: u8 = 0xa3;
pub const RFCOMM_MUX_FCON_RSP: u8 = 0xa1;
pub const RFCOMM_MUX_FCOFF_CMD: u8 = 0x63;
pub const RFCOMM_MUX_FCOFF_RSP: u8 = 0x61;
pub const RFCOMM_MUX_MSC_CMD: u8 = 0xe3;
pub const RFCOMM_MUX_MSC_RSP: u8 = 0xe1;
/// RFCOMM modem status signals byte advertising us as ready: EA(1) |
/// RTC(1) | RTR(1) | DV(1) — "Ready To Communicate / Ready To Receive
/// / Data Valid". Matches what BTstack ships in `rfcomm.c` as the HF
/// default local_modem_status, which most AGs (including iOS / Android)
/// have come to expect from a connecting hands-free unit.
pub const RFCOMM_LOCAL_MODEM_STATUS: u8 = 0x8d;
pub const RFCOMM_MUX_NSC_RSP: u8 = 0x11;
pub const RFCOMM_MUX_PN_CMD: u8 = 0x83;
pub const RFCOMM_MUX_PN_RSP: u8 = 0x81;

/// Pre-Bluetooth-1.0B PN frame type — declares "no credit-based flow
/// control on this DLCI". Used for HFP since the AG always sets it
/// and we just echo. Modern profiles (MAP/PBAP on Bluedroid) require
/// 0xe0 instead — see `RFCOMM_PN_BLUETOOTH_FRAME_TYPE_CFC`.
pub const RFCOMM_PN_BLUETOOTH_FRAME_TYPE: u8 = 0xf0;
/// Credit-based flow control PN frame type (post-Bluetooth-1.0B).
/// Bluedroid silently drops outbound MAS SABM unless our PN command
/// declares credit-based FC — even though the corresponding PN
/// response echoes with credits=0. Matches BTstack's
/// `RFCOMM_PN_FRAME_TYPE_UIH` (the comment in rfcomm.c calls it
/// "with cbfc: 0xe0").
pub const RFCOMM_PN_BLUETOOTH_FRAME_TYPE_CFC: u8 = 0xe0;
pub const RFCOMM_DEFAULT_MAX_FRAME_SIZE: u16 = 127;
/// Smallest `max_frame_size` we'll ever accept from a peer's PN. The
/// RFCOMM spec lets the field carry any 16-bit value, but anything
/// below ~23 bytes leaves no room for the PN reply itself, and a value
/// of 0 would crash later frame-encoding paths with a divide-by-zero.
/// 23 matches the L2CAP minimum MTU and is a safe lower bound.
pub const RFCOMM_MIN_MAX_FRAME_SIZE: u16 = 23;
const RFCOMM_CONTROL_POLL_FINAL: u8 = 0x10;
const RFCOMM_CRC8_INIT: u8 = 0xff;
const RFCOMM_CRC8_POLY_REVERSED: u8 = 0xe0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RfcommFrameKind {
    Sabm,
    Ua,
    Dm,
    Disc,
    Uih,
    Unknown(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RfcommFrame<'a> {
    pub dlci: u8,
    pub command_response: bool,
    pub kind: RfcommFrameKind,
    pub poll_final: bool,
    pub credits: Option<u8>,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RfcommMuxCommand<'a> {
    ParameterNegotiation {
        is_response: bool,
        dlci: u8,
        frame_type: u8,
        priority: u8,
        max_frame_size: u16,
        credits: u8,
    },
    ModemStatus {
        is_response: bool,
        dlci: u8,
        signals: u8,
    },
    FlowControlOn,
    FlowControlOff,
    Unknown {
        command_type: u8,
        payload: &'a [u8],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RfcommChannelState {
    Closed,
    Open,
}

/// A DLCI we (the MCE) are opening as initiator on an *existing*
/// inbound RFCOMM L2CAP channel. The phone (Pixel/Bluedroid) refuses to
/// accept a second L2CAP/PSM 0x0003 session per peer — additional
/// outbound L2CAP RFCOMM channels open at the L2CAP layer but their
/// multiplexer SABMs are silently dropped. Reusing the inbound
/// multiplexer (the one the phone established for HFP) is the only
/// path that works, and matches what BlueZ / BTstack do.
///
/// The state machine is the second half of `RfcommClientState`: the
/// multiplexer SABM/UA is already done (the phone did it), so we go
/// straight to PN → SABM(target) → UA → MSC ⇄ MSC RSP → Open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientDlciPhase {
    AwaitingPnRsp,
    AwaitingTargetUa,
    /// SABM acked, MSC CMD sent. Stays here until either the peer's
    /// MSC RSP comes back (clears `msc_local_pending`) AND we've replied
    /// to their MSC CMD (sets `msc_remote_received`).
    AwaitingMscExchange,
    Open,
    Failed(String),
}

#[derive(Debug)]
struct ClientDlci {
    phase: ClientDlciPhase,
    /// Set when our MSC CMD goes out; cleared on peer's MSC RSP.
    msc_local_pending: bool,
    /// Set when we reply to peer's MSC CMD. Some phones initiate MSC
    /// before responding to ours; both directions must complete before
    /// we declare the DLCI Open.
    msc_remote_received: bool,
}

/// Events produced by client-DLCI state transitions. Drained by the
/// runtime via `RfcommState::take_client_events` after each inbound
/// ACL packet so the runtime can advance its OBEX state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientDlciEvent {
    /// DLCI is fully open — both directions of MSC have settled. The
    /// runtime can now send UIH (OBEX) frames via
    /// `RfcommState::build_uih_on_client_dlci`.
    Opened { dlci: u8 },
    /// DLCI failed during open or after — peer sent DM, mux closed,
    /// etc. Runtime should propagate as a fatal error.
    Failed { dlci: u8, reason: String },
    /// UIH payload arrived on the DLCI. Runtime parses as OBEX and
    /// advances its MAS / PBAP session state.
    Payload { dlci: u8, payload: Vec<u8> },
}

// RfcommState contains closures via `extra_channel_handlers`, so we
// can no longer derive Debug/Clone/PartialEq/Eq. Nothing in the
// codebase uses those — tests assert via accessors only — so the
// derives are dropped. L2capChannel (which owns Option<RfcommState>)
// likewise drops them in lockstep.
pub struct RfcommState {
    multiplexer_open: bool,
    hfp_channel_state: RfcommChannelState,
    hfp_dlci: u8,
    max_frame_size: u16,
    hfp_state: hfp::HfpHandsFreeState,
    /// Incomplete trailing AT line carried across UIH frames (a response
    /// line can fragment at the mux MTU — see parse_ag_results_buffered).
    hfp_line_carry: String,
    hfp_pending_commands: VecDeque<hfp::HfpAtCommand>,
    /// Tracks the command that's currently waiting on the AG's OK/ERROR
    /// reply. `None` means there's no SLC command in flight (either we've
    /// already drained the queue and gone post-SLC, or we never sent the
    /// first one). The pre-fix code did not track this, so when the AG
    /// returned ERROR the queue silently stalled and SLC never readied.
    hfp_in_flight_command: Option<hfp::HfpAtCommand>,
    /// Wall-clock timestamp of when `hfp_in_flight_command` was sent.
    /// `tick_hfp_stall` uses this to fail the SLC if the AG drops a
    /// reply (or never sends OK/ERROR) — without it the queue stalls
    /// forever and the receptionist silently never goes service-level
    /// ready, which on the Pixel test phone happens roughly once a
    /// week as the AG drops a reply during congested L2CAP windows.
    hfp_in_flight_command_sent_at: Option<Instant>,
    hfp_slc_failed: bool,
    hfp_events: Vec<hfp::HfpEvent>,
    /// True after we send our MSC CMD and before the AG sends MSC RSP.
    /// HFP §4.2.1 requires MSC exchange in BOTH directions before AT
    /// commands flow; if we kick AT while either direction is still
    /// pending, the AG (iPhones definitely, others sometimes) drops
    /// the link. Pre-fix we sent AT+BRSF immediately after SABM/UA,
    /// causing the AG to DISC right after sending its MSC CMD.
    msc_local_pending: bool,
    /// True once we've received the AG's MSC CMD (and replied with MSC
    /// RSP). Pairs with `msc_local_pending` to know when MSC exchange
    /// is fully done — the moment both flip to "completed" we kick the
    /// AT command queue.
    msc_remote_received: bool,
    /// Latches once we've kicked the AT command queue so a stray
    /// duplicate MSC from the AG can't cause us to re-send AT+BRSF.
    msc_at_kicked: bool,
    /// Server-channel registry for *additional* profiles beyond HFP.
    /// Keyed by DLCI (not channel number) so the dispatch in
    /// `handle_sabm`/`handle_uih`/`handle_disc` can match directly.
    /// Empty by default; future profiles populate via
    /// `register_server_channel`.
    extra_channel_handlers: BTreeMap<u8, ServerChannelHandlers>,
    /// Outbound DLCIs we're opening on this multiplexer (we're the
    /// initiator). Keyed by target DLCI. See `ClientDlci` for the
    /// per-DLCI state machine. Populated via `attach_client_dlci`.
    client_dlcis: BTreeMap<u8, ClientDlci>,
    /// Events produced by client-DLCI transitions, drained by the
    /// runtime via `take_client_events`.
    client_events: Vec<ClientDlciEvent>,
    /// Whether wide-band speech (mSBC) should be advertised in AT+BAC.
    /// Set by the runtime from `WinUsbBluetoothTransport::supports_msbc_alt_setting`
    /// before the SLC sequence kicks. Defaults to false so a misconfigured
    /// transport never offers mSBC.
    wbs_supported: bool,
}

impl Default for RfcommState {
    fn default() -> Self {
        Self {
            multiplexer_open: false,
            hfp_channel_state: RfcommChannelState::Closed,
            hfp_dlci: aokie_hfp_dlci(),
            max_frame_size: RFCOMM_DEFAULT_MAX_FRAME_SIZE,
            hfp_state: hfp::HfpHandsFreeState::new(),
            hfp_line_carry: String::new(),
            hfp_pending_commands: VecDeque::new(),
            hfp_in_flight_command: None,
            hfp_in_flight_command_sent_at: None,
            hfp_slc_failed: false,
            hfp_events: Vec::new(),
            msc_local_pending: false,
            msc_remote_received: false,
            msc_at_kicked: false,
            extra_channel_handlers: BTreeMap::new(),
            client_dlcis: BTreeMap::new(),
            client_events: Vec::new(),
            wbs_supported: false,
        }
    }
}

impl RfcommState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the RFCOMM state whether wide-band speech (mSBC) should be
    /// advertised when the SLC AT queue fires AT+BAC. Caller is the
    /// runtime, which derives this from
    /// `WinUsbBluetoothTransport::supports_msbc_alt_setting`. Must be
    /// called *before* `handle_sabm` builds the SLC queue — calling
    /// after that point has no effect on the in-flight session.
    pub fn set_wbs_supported(&mut self, wbs_supported: bool) {
        self.wbs_supported = wbs_supported;
    }

    /// Phase 4: whether this SLC advertises call waiting / 3-way (BRSF bit 1)
    /// and probes AT+CHLD=? / AT+CCWA=1. Same set-before-SABM contract as
    /// `set_wbs_supported`; the runtime derives it from AOKIE_CALL_WAITING.
    pub fn set_call_waiting_enabled(&mut self, enabled: bool) {
        self.hfp_state.set_call_waiting_enabled(enabled);
    }

    /// Attach a new outbound DLCI on this RFCOMM multiplexer. Returns
    /// the target DLCI and the PN command framed as a UIH-on-DLCI-0
    /// ready to be wrapped by the L2CAP layer. The runtime sends this
    /// on the L2CAP channel, then waits for `ClientDlciEvent::Opened`
    /// before sending OBEX.
    ///
    /// Errors if the multiplexer isn't open yet (caller should ensure
    /// the inbound HFP RFCOMM is up first) or if the DLCI is already
    /// in use.
    pub fn attach_client_dlci(&mut self, server_channel: u8) -> Result<(u8, Vec<u8>), String> {
        if !self.multiplexer_open {
            return Err(
                "attach_client_dlci called before multiplexer SABM/UA — no shared mux to ride on"
                    .to_string(),
            );
        }
        // D-bit = 1. Empirically required by Bluedroid: the spec /
        // BlueZ formula says D should reflect "is this side the
        // multiplexer initiator?" (which would give D=0 here, since
        // the phone opened the mux). But on Pixel/Bluedroid, sending
        // PN+SABM with D=0 gets a clean PN response *echoing* DLCI 10,
        // followed by silence on the SABM — Bluedroid registers its
        // MAS service under the D=1 form (DLCI = 2*scn + 1) and the
        // SABM lookup misses. Flipping to D=1 makes the SABM hit the
        // service's internal slot. The legacy `RfcommClientState`
        // already uses D=1 (it owns the mux there, so the rule and
        // the empirical answer agree); shared-mux is the case where
        // they diverge, and the empirical answer wins.
        let target_dlci = server_channel_dlci(server_channel, true);
        if self.client_dlcis.contains_key(&target_dlci)
            || target_dlci == self.hfp_dlci
            || self.extra_channel_handlers.contains_key(&target_dlci)
        {
            return Err(format!(
                "attach_client_dlci: dlci {} already in use",
                target_dlci
            ));
        }
        self.client_dlcis.insert(
            target_dlci,
            ClientDlci {
                phase: ClientDlciPhase::AwaitingPnRsp,
                msc_local_pending: false,
                msc_remote_received: false,
            },
        );
        // Priority 7 + max_frame_size matching the negotiated mux —
        // mirrors RfcommClientState::on_ua_multiplexer for parity.
        // Initial credits = 7 (matches BTstack/BlueZ defaults). Use
        // the credit-based-FC PN frame type (0xe0): Bluedroid acks
        // legacy 0xf0 PN but then silently drops the SABM that
        // follows, so we have to declare CFC up front to actually
        // open a shared-mux DLCI on Pixel.
        let pn = build_parameter_negotiation_command_cfc(target_dlci, 7, self.max_frame_size, 7);
        // C/R = 0 because we (the shared-mux *responder*) are sending a
        // command. RFCOMM v1.2 §5.4.3.1: cr = mux_initiator XOR is_response.
        // The phone initiated the mux, so we're the responder; our PN
        // command has cr=0. Sending cr=1 here makes Bluedroid log
        // "Bad UIH - response" and silently drop the frame.
        let frame = build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &pn);
        eprintln!(
            "[AokieRadio] RFCOMM client DLCI {} attached — PN frame={:02x?}",
            target_dlci, frame
        );
        Ok((target_dlci, frame))
    }

    /// Drain pending client-DLCI events. Runtime calls after each
    /// inbound ACL packet so it can react to Opened / Failed / Payload.
    /// **Warning:** drains events for ALL client DLCIs on the mux. Two
    /// runtimes sharing the same RFCOMM mux (e.g. MAP + PBAP both riding
    /// the inbound HFP channel) MUST use `take_client_events_for_dlci`
    /// instead — otherwise whichever ticks first eats the other's
    /// events and the second runtime stalls forever.
    pub fn take_client_events(&mut self) -> Vec<ClientDlciEvent> {
        std::mem::take(&mut self.client_events)
    }

    /// Drain pending client-DLCI events whose `dlci` matches. Events
    /// for other DLCIs stay in the queue so a sibling runtime can pick
    /// them up on its own tick. This is the right entrypoint for
    /// shared-mux runtimes (MAP, PBAP) which each own a single DLCI.
    pub fn take_client_events_for_dlci(&mut self, dlci: u8) -> Vec<ClientDlciEvent> {
        let all = std::mem::take(&mut self.client_events);
        let mut taken = Vec::new();
        let mut remaining = Vec::new();
        for ev in all {
            let ev_dlci = match &ev {
                ClientDlciEvent::Opened { dlci } => *dlci,
                ClientDlciEvent::Failed { dlci, .. } => *dlci,
                ClientDlciEvent::Payload { dlci, .. } => *dlci,
            };
            if ev_dlci == dlci {
                taken.push(ev);
            } else {
                remaining.push(ev);
            }
        }
        self.client_events = remaining;
        taken
    }

    /// True when at least one client DLCI is in any non-terminal phase
    /// — used by the orchestrator to decide whether to keep waiting on
    /// inbound traffic to advance the state machine.
    pub fn has_active_client_dlci(&self) -> bool {
        self.client_dlcis
            .values()
            .any(|c| !matches!(c.phase, ClientDlciPhase::Failed(_)))
    }

    /// Build a UIH frame on `dlci` carrying `payload` (typically OBEX
    /// bytes from the runtime). Errors if `dlci` isn't a tracked client
    /// DLCI in `Open` state.
    pub fn build_uih_on_client_dlci(&self, dlci: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        let client = self
            .client_dlcis
            .get(&dlci)
            .ok_or_else(|| format!("build_uih_on_client_dlci: dlci {} not tracked", dlci))?;
        if !matches!(client.phase, ClientDlciPhase::Open) {
            return Err(format!(
                "build_uih_on_client_dlci: dlci {} not Open (phase {:?})",
                dlci, client.phase
            ));
        }
        // C/R = 0: we ride the inbound HFP multiplexer as responder, so
        // any UIH information frame we send (OBEX commands carrying
        // payload) follows the spec rule cr = mux_initiator XOR
        // is_response = 0 XOR 0 = 0. Bluedroid logs "Bad UIH - response"
        // and drops the frame if cr=1 here.
        Ok(build_uih(dlci, false, None, payload))
    }

    /// Tear down a client DLCI by sending DISC. Caller wraps the
    /// returned bytes in L2CAP. Removes the DLCI from tracking.
    pub fn build_disc_for_client_dlci(&mut self, dlci: u8) -> Result<Vec<u8>, String> {
        if self.client_dlcis.remove(&dlci).is_none() {
            return Err(format!(
                "build_disc_for_client_dlci: dlci {} not tracked",
                dlci
            ));
        }
        // C/R = 0: DISC is a command issued by us (mux responder).
        Ok(build_disc(dlci, false))
    }

    /// Force-tear-down ANY DLCI (client or server) we know about, used
    /// by the runtime's MAS-stall recovery path. Idempotent: if the
    /// DLCI isn't currently tracked we still emit the DISC frame so a
    /// half-open peer-side state can clear.
    ///
    /// For a CLIENT DLCI (we initiated) we drop tracking so the next
    /// `attach_client_dlci` for the same DLCI doesn't trip the
    /// "already in use" guard — the client side has to re-PN/re-SABM
    /// from scratch.
    ///
    /// For a SERVER DLCI (peer initiates) we MUST keep the handler
    /// registered. The recovery path is "force-DISC, peer reopens" —
    /// when Pixel sends PN to reopen MNS dlci 4, our PN handler does
    /// `extra_channel_handlers.contains_key(&dlci)` and replies NSC
    /// if the handler is gone. Removing it locks Pixel out of the
    /// channel for the rest of the ACL, breaks MNS, and the link
    /// silently degrades to dead-air.
    pub fn build_force_disc_dlci(&mut self, dlci: u8) -> Vec<u8> {
        self.client_dlcis.remove(&dlci);
        // Same C/R rule as the client-side variant: we're the mux
        // responder, so every command we emit rides cr=0.
        build_disc(dlci, false)
    }

    /// Register a non-HFP server channel. The peer's PN/SABM on the
    /// matching DLCI will route into the supplied closures. HFP keeps
    /// using its bespoke inline path because its SLC/MSC/AT state is
    /// woven through the rest of `RfcommState`; future profiles like
    /// MAP/PBAP are clean enough to live behind this seam.
    pub fn register_server_channel(&mut self, server_channel: u8, handlers: ServerChannelHandlers) {
        // We act as the server, so the AG (mux initiator) sees outgoing=1
        // and we therefore listen on D-bit=0 — same calculation as
        // `aokie_hfp_dlci`. Done at registration time so the dispatch
        // loop can do a direct DLCI lookup.
        let dlci = server_channel_dlci(server_channel, false);
        self.extra_channel_handlers.insert(dlci, handlers);
    }

    pub fn multiplexer_open(&self) -> bool {
        self.multiplexer_open
    }

    pub fn hfp_channel_state(&self) -> RfcommChannelState {
        self.hfp_channel_state
    }

    pub fn hfp_state(&self) -> &hfp::HfpHandsFreeState {
        &self.hfp_state
    }

    pub fn hfp_events(&self) -> &[hfp::HfpEvent] {
        &self.hfp_events
    }

    pub fn take_hfp_events(&mut self) -> Vec<hfp::HfpEvent> {
        std::mem::take(&mut self.hfp_events)
    }

    pub fn build_call_control_command(&mut self, command: hfp::HfpAtCommand) -> Option<Vec<u8>> {
        if self.hfp_channel_state != RfcommChannelState::Open {
            return None;
        }
        Some(self.build_hfp_command_frame(command))
    }

    pub fn handle_packet(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let frame = parse_frame(packet)?;
        eprintln!(
            "[AokieRadio] RFCOMM frame in: kind={:?} dlci={} cr={} pf={} payload={}B",
            frame.kind,
            frame.dlci,
            frame.command_response,
            frame.poll_final,
            frame.payload.len()
        );
        match frame.kind {
            RfcommFrameKind::Sabm => Ok(self.handle_sabm(frame.dlci)),
            RfcommFrameKind::Ua if self.client_dlcis.contains_key(&frame.dlci) => {
                Ok(self.on_client_target_ua(frame.dlci))
            }
            RfcommFrameKind::Dm if self.client_dlcis.contains_key(&frame.dlci) => {
                self.fail_client_dlci(
                    frame.dlci,
                    format!("peer sent DM on outbound dlci {}", frame.dlci),
                );
                Ok(Vec::new())
            }
            RfcommFrameKind::Disc => Ok(vec![self.handle_disc(frame.dlci)]),
            RfcommFrameKind::Uih if frame.dlci == RFCOMM_DLCI_MULTIPLEXER => {
                self.handle_multiplexer_payload(frame.payload)
            }
            RfcommFrameKind::Uih if frame.dlci == self.hfp_dlci => {
                self.handle_hfp_payload(frame.payload)
            }
            RfcommFrameKind::Uih if self.client_dlcis.contains_key(&frame.dlci) => {
                self.client_events.push(ClientDlciEvent::Payload {
                    dlci: frame.dlci,
                    payload: frame.payload.to_vec(),
                });
                Ok(Vec::new())
            }
            RfcommFrameKind::Uih => {
                // Phase 0c registry path: route to a registered tenant
                // (MAP/PBAP/etc.) if one owns this DLCI. Cloning the
                // Arc is cheap and keeps `&mut self` free.
                if let Some(handlers) = self.extra_channel_handlers.get(&frame.dlci).cloned() {
                    return (handlers.on_uih)(frame.payload);
                }
                eprintln!(
                    "[AokieRadio] RFCOMM UIH ignored — dlci {} not multiplexer (0), HFP ({}), or in extra-channel registry",
                    frame.dlci, self.hfp_dlci
                );
                Ok(Vec::new())
            }
            _ => {
                eprintln!(
                    "[AokieRadio] RFCOMM frame ignored — kind {:?} on dlci {}",
                    frame.kind, frame.dlci
                );
                Ok(Vec::new())
            }
        }
    }

    /// UA on a client-owned target DLCI = our SABM was accepted. Send
    /// our MSC CMD on the multiplexer (signals 0x8d, same as HFP) and
    /// move to AwaitingMscExchange. Once both directions of MSC settle
    /// the DLCI flips to Open and we emit `ClientDlciEvent::Opened`.
    fn on_client_target_ua(&mut self, dlci: u8) -> Vec<Vec<u8>> {
        let Some(client) = self.client_dlcis.get_mut(&dlci) else {
            return Vec::new();
        };
        if !matches!(client.phase, ClientDlciPhase::AwaitingTargetUa) {
            eprintln!(
                "[AokieRadio] RFCOMM client UA on dlci {} ignored — phase {:?}",
                dlci, client.phase
            );
            return Vec::new();
        }
        client.phase = ClientDlciPhase::AwaitingMscExchange;
        client.msc_local_pending = true;
        client.msc_remote_received = false;
        let msc = build_modem_status_command(dlci, RFCOMM_LOCAL_MODEM_STATUS);
        eprintln!(
            "[AokieRadio] RFCOMM client DLCI {} UA acked — sending MSC CMD",
            dlci
        );
        // C/R = 0: mux-responder issuing a command. See attach_client_dlci
        // for the spec citation. Sending cr=1 here would also trip
        // Bluedroid's "Bad UIH - response" parser check.
        vec![build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &msc)]
    }

    fn fail_client_dlci(&mut self, dlci: u8, reason: String) {
        if let Some(client) = self.client_dlcis.get_mut(&dlci) {
            client.phase = ClientDlciPhase::Failed(reason.clone());
        }
        eprintln!(
            "[AokieRadio] RFCOMM client DLCI {} FAILED: {}",
            dlci, reason
        );
        self.client_events
            .push(ClientDlciEvent::Failed { dlci, reason });
    }

    fn maybe_open_client_dlci(&mut self, dlci: u8) {
        let Some(client) = self.client_dlcis.get_mut(&dlci) else {
            return;
        };
        if matches!(client.phase, ClientDlciPhase::AwaitingMscExchange)
            && !client.msc_local_pending
            && client.msc_remote_received
        {
            client.phase = ClientDlciPhase::Open;
            eprintln!("[AokieRadio] RFCOMM client DLCI {} OPEN", dlci);
            self.client_events.push(ClientDlciEvent::Opened { dlci });
        }
    }

    fn handle_sabm(&mut self, dlci: u8) -> Vec<Vec<u8>> {
        if dlci == RFCOMM_DLCI_MULTIPLEXER {
            eprintln!("[AokieRadio] RFCOMM SABM on multiplexer DLCI 0 — opening session");
            self.multiplexer_open = true;
            return vec![build_ua(dlci, true)];
        }

        // Phase 0c registry path: a registered non-HFP tenant gets to
        // drive its own SABM response. Checked *before* the unknown-DLCI
        // DM branch below so future MAP/PBAP channels don't trip the
        // "send DM" fallback.
        if self.multiplexer_open {
            if let Some(handlers) = self.extra_channel_handlers.get(&dlci).cloned() {
                eprintln!(
                    "[AokieRadio] RFCOMM SABM on DLCI {} — registered extra channel",
                    dlci
                );
                return (handlers.on_sabm)();
            }
        }

        if self.multiplexer_open && dlci == self.hfp_dlci {
            eprintln!(
                "[AokieRadio] RFCOMM SABM on HFP DLCI {} — channel open, sending MSC CMD before SLC",
                dlci
            );
            self.hfp_channel_state = RfcommChannelState::Open;
            self.hfp_slc_failed = false;
            self.hfp_in_flight_command = None;
            self.msc_local_pending = true;
            self.msc_remote_received = false;
            self.msc_at_kicked = false;
            // Queue the SLC AT commands — they fire only after the MSC
            // exchange completes in both directions (per HFP §4.2.1).
            self.hfp_pending_commands = hfp::HfpHandsFreeState::initial_service_level_commands(
                self.wbs_supported,
                self.hfp_state.call_waiting_enabled(),
            )
            .into();
            // Reply with UA, then proactively send our MSC CMD on the
            // multiplexer DLCI so the AG knows we're ready to talk.
            // RFCOMM signal byte 0x8d = EA(1) | RTC(1) | RTR(1) | DV(1)
            // — "ready to communicate / ready to receive / data valid".
            vec![
                build_ua(dlci, true),
                build_uih(
                    RFCOMM_DLCI_MULTIPLEXER,
                    false,
                    None,
                    &build_modem_status_command(dlci, RFCOMM_LOCAL_MODEM_STATUS),
                ),
            ]
        } else {
            eprintln!(
                "[AokieRadio] RFCOMM SABM on unexpected DLCI {} (multiplexer_open={}, hfp_dlci={}) — sending DM",
                dlci, self.multiplexer_open, self.hfp_dlci
            );
            vec![build_dm(dlci, true)]
        }
    }

    fn maybe_kick_at_sequence(&mut self) -> Vec<Vec<u8>> {
        if self.msc_at_kicked
            || self.msc_local_pending
            || !self.msc_remote_received
            || self.hfp_channel_state != RfcommChannelState::Open
        {
            return Vec::new();
        }
        self.msc_at_kicked = true;
        eprintln!("[AokieRadio] RFCOMM MSC exchange complete — kicking HFP SLC sequence");
        if let Some(command) = self.next_hfp_command_frame() {
            return vec![command];
        }
        Vec::new()
    }

    fn handle_disc(&mut self, dlci: u8) -> Vec<u8> {
        if dlci == RFCOMM_DLCI_MULTIPLEXER {
            self.multiplexer_open = false;
            self.hfp_channel_state = RfcommChannelState::Closed;
        } else if dlci == self.hfp_dlci {
            self.hfp_channel_state = RfcommChannelState::Closed;
        } else if let Some(handlers) = self.extra_channel_handlers.get(&dlci).cloned() {
            // Let the registered tenant clean up its own state, then
            // use whatever UA frame it returns. (Most tenants will
            // return the standard build_ua, but this leaves them room
            // to layer in OBEX disconnect bookkeeping.)
            return (handlers.on_disc)();
        }

        build_ua(dlci, true)
    }

    fn handle_multiplexer_payload(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        if !self.multiplexer_open {
            eprintln!(
                "[AokieRadio] RFCOMM multiplexer payload received but mux not open — dropping"
            );
            return Ok(Vec::new());
        }

        let command = parse_multiplexer_command(payload)?;
        let mut out: Vec<Vec<u8>> = Vec::new();
        let response = match command {
            RfcommMuxCommand::ParameterNegotiation {
                is_response,
                dlci,
                frame_type,
                priority,
                max_frame_size,
                credits,
            } => {
                if is_response {
                    // PN response only matters when we originated the
                    // PN (client-DLCI path). For HFP we never originate
                    // PN, so a stray response there is silently swallowed.
                    if let Some(client) = self.client_dlcis.get_mut(&dlci) {
                        if matches!(client.phase, ClientDlciPhase::AwaitingPnRsp) {
                            self.max_frame_size = max_frame_size
                                .clamp(RFCOMM_MIN_MAX_FRAME_SIZE, RFCOMM_DEFAULT_MAX_FRAME_SIZE);
                            client.phase = ClientDlciPhase::AwaitingTargetUa;
                            // C/R = 0: we're the mux responder issuing a
                            // command (SABM is always a command). Same
                            // direction-bit rule as the PN above —
                            // sending cr=1 from a mux-responder yields
                            // Bluedroid's "Bad SABME" parser rejection.
                            let sabm = build_sabm(dlci, false);
                            eprintln!(
                                "[AokieRadio] RFCOMM client DLCI {} PN acked (frame_type=0x{:02x}, max_frame_size={}, credits={}) — sending target SABM bytes={:02x?}",
                                dlci, frame_type, self.max_frame_size, credits, sabm
                            );
                            out.push(sabm);
                        }
                    } else {
                        eprintln!(
                            "[AokieRadio] RFCOMM PN response received in server mode for dlci {} — ignoring",
                            dlci
                        );
                    }
                    None
                } else if dlci != self.hfp_dlci && !self.extra_channel_handlers.contains_key(&dlci)
                {
                    let registered: Vec<u8> = self.extra_channel_handlers.keys().copied().collect();
                    eprintln!(
                        "[AokieRadio] RFCOMM PN target dlci {} not registered — replying NSC (hfp_dlci={}, registered_extras={:?})",
                        dlci, self.hfp_dlci, registered
                    );
                    Some(build_nsc_response(RFCOMM_MUX_PN_CMD))
                } else {
                    // PN on either HFP or a registered extra channel
                    // (MAP MNS, MAP MAS server, etc.) — accept and
                    // mirror the AG's negotiated max_frame_size for
                    // *this* mux. Clamp identically to HFP so a
                    // pathological PN can't underflow our framer.
                    self.max_frame_size = max_frame_size
                        .clamp(RFCOMM_MIN_MAX_FRAME_SIZE, RFCOMM_DEFAULT_MAX_FRAME_SIZE);
                    Some(build_parameter_negotiation_response(
                        dlci,
                        priority,
                        self.max_frame_size,
                        credits,
                    ))
                }
            }
            RfcommMuxCommand::ModemStatus {
                dlci,
                signals,
                is_response,
            } => {
                if is_response {
                    if dlci == self.hfp_dlci {
                        eprintln!(
                            "[AokieRadio] RFCOMM MSC RSP for dlci {} — our MSC CMD acknowledged",
                            dlci
                        );
                        self.msc_local_pending = false;
                    } else if let Some(client) = self.client_dlcis.get_mut(&dlci) {
                        client.msc_local_pending = false;
                        eprintln!(
                            "[AokieRadio] RFCOMM client DLCI {} MSC RSP — local pending cleared",
                            dlci
                        );
                        self.maybe_open_client_dlci(dlci);
                    }
                    None
                } else if dlci == self.hfp_dlci {
                    eprintln!(
                        "[AokieRadio] RFCOMM MSC CMD for dlci {} signals 0x{:02x} — replying MSC RSP",
                        dlci, signals
                    );
                    self.msc_remote_received = true;
                    Some(build_modem_status_response(dlci, signals))
                } else if self.client_dlcis.contains_key(&dlci) {
                    // Peer's MSC CMD on our outbound DLCI. Reply MSC
                    // RSP and remember we did so — `maybe_open_client_dlci`
                    // promotes to Open if our own MSC has also been acked.
                    eprintln!(
                        "[AokieRadio] RFCOMM client DLCI {} MSC CMD signals 0x{:02x} — replying MSC RSP",
                        dlci, signals
                    );
                    if let Some(client) = self.client_dlcis.get_mut(&dlci) {
                        client.msc_remote_received = true;
                    }
                    let msc_rsp = build_modem_status_response(dlci, signals);
                    self.maybe_open_client_dlci(dlci);
                    Some(msc_rsp)
                } else if self.extra_channel_handlers.contains_key(&dlci) {
                    // Registered tenant (MAP MNS, etc.) — most phones
                    // expect an MSC RSP before they'll send UIH on the
                    // channel. We don't track local pending state for
                    // extras (we don't proactively send MSC CMD on
                    // their behalf today), so this is purely
                    // protocol-correctness.
                    eprintln!(
                        "[AokieRadio] RFCOMM MSC CMD for registered dlci {} signals 0x{:02x} — replying MSC RSP",
                        dlci, signals
                    );
                    Some(build_modem_status_response(dlci, signals))
                } else {
                    Some(build_nsc_response(RFCOMM_MUX_MSC_CMD))
                }
            }
            RfcommMuxCommand::FlowControlOn => {
                Some(build_empty_multiplexer_response(RFCOMM_MUX_FCON_RSP))
            }
            RfcommMuxCommand::FlowControlOff => {
                Some(build_empty_multiplexer_response(RFCOMM_MUX_FCOFF_RSP))
            }
            RfcommMuxCommand::Unknown { command_type, .. } => {
                Some(build_nsc_response(command_type))
            }
        };

        if let Some(payload) = response {
            out.push(build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &payload));
        }
        // After processing this multiplexer message, the MSC bookkeeping
        // may have flipped to "exchange complete" — kick the AT command
        // queue if so. Has to happen here (not in handle_sabm) because
        // the AG can send its MSC CMD either before or after the RSP we
        // expect from our own MSC CMD, and we want to ATA either way.
        out.extend(self.maybe_kick_at_sequence());
        Ok(out)
    }

    fn handle_hfp_payload(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        if self.hfp_channel_state != RfcommChannelState::Open {
            return Ok(Vec::new());
        }

        let mut responses = Vec::new();
        for result in hfp::parse_ag_results_buffered(&mut self.hfp_line_carry, payload)? {
            self.hfp_events.extend(self.hfp_state.apply_result(&result));
            match result {
                hfp::HfpAgResult::Ok => {
                    if matches!(
                        self.hfp_in_flight_command,
                        Some(hfp::HfpAtCommand::EnableCallWaitingNotifications)
                    ) {
                        self.hfp_state.note_ccwa_accepted();
                    }
                    self.hfp_in_flight_command = None;
                    self.hfp_in_flight_command_sent_at = None;
                    if !self.hfp_slc_failed {
                        self.advance_hfp_slc_queue(&mut responses);
                    }
                }
                hfp::HfpAgResult::Error => {
                    // ERROR replies during SLC bring the whole sequence
                    // down — there's no spec-defined retry that's safe to
                    // attempt blind, and stalling silently would leave
                    // the channel in an indeterminate state forever. Drop
                    // the queue, surface a fatal event, and let the
                    // manager tear the RFCOMM channel down. Post-SLC
                    // ERRORs (e.g. "ATA" while no call is incoming) are
                    // surfaced too but do not flip the SLC-ready bit.
                    //
                    // Phase 4 exception: the call-waiting capability probes
                    // (AT+CHLD=? / AT+CCWA=1) are NON-fatal — a phone that
                    // refuses them merely lacks call waiting, and losing the
                    // whole HFP connection over an optional capability was
                    // the first bug of the Phase-4 review. The queue
                    // continues exactly as if the probe had succeeded.
                    let failed_command = self.hfp_in_flight_command.take();
                    self.hfp_in_flight_command_sent_at = None;
                    let capability_probe = failed_command
                        .as_ref()
                        .is_some_and(|c| self.hfp_state.note_slc_probe_error(c));
                    if capability_probe {
                        if !self.hfp_slc_failed {
                            self.advance_hfp_slc_queue(&mut responses);
                        }
                    } else {
                        if !self.hfp_state.service_level_ready() {
                            self.hfp_pending_commands.clear();
                            self.hfp_slc_failed = true;
                        }
                        let label = failed_command
                            .as_ref()
                            .map(describe_at_command)
                            .unwrap_or("unsolicited")
                            .to_string();
                        self.hfp_events
                            .push(hfp::HfpEvent::ServiceLevelConnectionFailed(label));
                    }
                }
                hfp::HfpAgResult::SelectedCodec(codec) => {
                    responses
                        .push(self.build_hfp_command_frame(hfp::HfpAtCommand::ConfirmCodec(codec)));
                }
                _ => {}
            }
        }
        Ok(responses)
    }

    fn next_hfp_command_frame(&mut self) -> Option<Vec<u8>> {
        loop {
            let command = self.hfp_pending_commands.pop_front()?;
            // Phase 4: the call-waiting probes only go out when both sides
            // advertise three-way calling (the AG's +BRSF lands before
            // either would be popped) — skipping keeps unsupported phones
            // byte-for-byte on the legacy SLC.
            if self.hfp_state.should_skip_slc_command(&command) {
                continue;
            }
            let frame = self.build_hfp_command_frame(command.clone());
            self.hfp_in_flight_command = Some(command);
            self.hfp_in_flight_command_sent_at = Some(Instant::now());
            return Some(frame);
        }
    }

    /// Shared SLC-queue advance: send the next command, or run the one-shot
    /// indicator-definitions retry, or declare readiness. Used by the OK arm
    /// and by the non-fatal capability-probe ERROR arm (which must continue
    /// the queue exactly as if the probe had succeeded).
    fn advance_hfp_slc_queue(&mut self, responses: &mut Vec<Vec<u8>>) {
        if let Some(command) = self.next_hfp_command_frame() {
            responses.push(command);
        } else if self.hfp_pending_commands.is_empty()
            && self.hfp_state.needs_indicator_definitions_retry()
        {
            // Lost/garbled +CIND=? definitions (phantom-answer
            // incidents): re-request ONCE before readiness —
            // ringing on default indices misreads as answered.
            self.hfp_pending_commands
                .push_back(hfp::HfpAtCommand::RetrieveIndicators);
            self.hfp_pending_commands
                .push_back(hfp::HfpAtCommand::RetrieveIndicatorStatus);
            if let Some(command) = self.next_hfp_command_frame() {
                responses.push(command);
            }
        } else {
            self.mark_hfp_service_ready();
        }
    }

    /// Check whether the in-flight SLC command has been outstanding for
    /// longer than `timeout`. If so, latch SLC failure and surface a
    /// `ServiceLevelConnectionFailed` event so the manager tears the
    /// channel down. Returns `true` if the stall fired this tick (so
    /// the caller can log once instead of every tick after).
    pub fn tick_hfp_stall(&mut self, now: Instant, timeout: Duration) -> bool {
        let Some(sent_at) = self.hfp_in_flight_command_sent_at else {
            return false;
        };
        if now.saturating_duration_since(sent_at) < timeout {
            return false;
        }
        let stalled = self
            .hfp_in_flight_command
            .take()
            .as_ref()
            .map(describe_at_command)
            .unwrap_or("(unknown)")
            .to_string();
        self.hfp_in_flight_command_sent_at = None;
        if !self.hfp_state.service_level_ready() {
            self.hfp_pending_commands.clear();
            self.hfp_slc_failed = true;
        }
        self.hfp_events
            .push(hfp::HfpEvent::ServiceLevelConnectionFailed(format!(
                "{} timed out after {:?}",
                stalled, timeout
            )));
        true
    }

    fn build_hfp_command_frame(&mut self, command: hfp::HfpAtCommand) -> Vec<u8> {
        self.hfp_state.mark_command_sent(&command);
        build_uih(self.hfp_dlci, true, None, &hfp::build_at_command(command))
    }

    fn mark_hfp_service_ready(&mut self) {
        if let Some(event) = self.hfp_state.mark_service_level_ready() {
            self.hfp_events.push(event);
        }
    }
}

/// Initiator-side RFCOMM state machine. Phase 2c of the MAP/PBAP plan:
/// once an L2CAP channel to PSM_RFCOMM is opened *outbound*, the runtime
/// installs an `RfcommClientState` and feeds it the raw RFCOMM frames
/// arriving on the channel. The state machine drives the multiplexer
/// SABM → PN → server-channel SABM → MSC handshake, then exposes a
/// `Payload` event for each UIH on the negotiated DLCI so PBAP/MAP
/// (which run OBEX over those bytes) can be entirely transport-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RfcommClientPhase {
    Idle,
    AwaitingMultiplexerUa,
    AwaitingPnResponse,
    AwaitingTargetUa,
    Open,
    Closed,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RfcommClientEvent {
    /// Channel reached `Open` — both MSC directions are settled and the
    /// caller can now start sending OBEX (or whatever upper-layer
    /// protocol) frames via `build_outbound_uih`.
    Opened,
    /// A UIH frame arrived on the negotiated DLCI. Payload is owned so
    /// the caller can buffer it across reassembly steps.
    Payload(Vec<u8>),
    /// Peer initiated DISC or otherwise closed the channel cleanly.
    Closed,
    /// Handshake or runtime error. Channel will not transition to
    /// `Open` — caller should tear down the L2CAP channel.
    Failed(String),
}

#[derive(Debug)]
pub struct RfcommClientState {
    server_channel: u8,
    target_dlci: u8,
    max_frame_size: u16,
    phase: RfcommClientPhase,
    /// True after our MSC CMD on the target DLCI is sent and before the
    /// peer's MSC RSP comes back. Pairs with `msc_remote_received` so
    /// `Opened` only fires once both directions are settled, matching
    /// the conservative HF/HFP behavior the existing server-mode state
    /// uses.
    msc_local_pending: bool,
    /// True once we've replied to the peer's MSC CMD with our MSC RSP.
    msc_remote_received: bool,
    pending_events: Vec<RfcommClientEvent>,
    /// Credit-based flow control negotiated (the peer's PN response
    /// carried the CFC-accepted frame type 0xe0). Bluedroid ALWAYS runs
    /// CFC and obeys it strictly for its own transmissions — the
    /// HARD-001 outbound-SLC failure was the AG queuing +BRSF forever
    /// because our PN granted it ZERO initial credits and we never
    /// topped it up: it DISC'd the DLCI after its 5s SLC timer.
    credit_flow: bool,
    /// Credits WE may spend on target-DLCI data frames (granted by the
    /// peer's PN response + inbound credit bytes). Only meaningful when
    /// `credit_flow`.
    tx_credits: u16,
    /// Credits the PEER currently holds toward us (granted by our PN
    /// command + our top-up frames). Refilled as data arrives so the
    /// peer never stalls the way the AG stalled on us.
    rx_credits_outstanding: u16,
    /// Secondary outbound DLCIs riding THIS (initiator) multiplexer —
    /// MAP MAS / PBAP PSE attach here when the outbound HFP client owns
    /// the mux. Bluedroid allows ONE RFCOMM session per peer: a second
    /// L2CAP/PSM-0x0003 session gets its L2CAP config acked and then
    /// the mux SABM is never answered (live 2026-07-13: the first
    /// kickoff SMS died in exactly that stall, and the phone rebound
    /// its HFP traffic onto the dead session's channel, deafening call
    /// handling). Keyed by target DLCI.
    secondary_dlcis: BTreeMap<u8, SecondaryDlci>,
    /// Events from secondary-DLCI transitions, drained by the sharing
    /// runtimes (MAP/PBAP) via `take_client_events_for_dlci` — the same
    /// contract `RfcommState` offers on the inbound mux, so the OBEX
    /// runtimes are mux-role agnostic.
    client_events: Vec<ClientDlciEvent>,
}

/// One secondary outbound DLCI on the initiator mux. The same
/// PN → SABM → MSC ⇄ MSC state walk as `RfcommState::ClientDlci`, plus
/// per-DLCI credit budgets (the primary DLCI's credits must never be
/// spent on OBEX frames or vice versa).
#[derive(Debug)]
struct SecondaryDlci {
    phase: ClientDlciPhase,
    msc_local_pending: bool,
    msc_remote_received: bool,
    credit_flow: bool,
    tx_credits: u16,
    rx_credits_outstanding: u16,
}

/// Initial credits we grant the peer in the PN command (BTstack/BlueZ
/// default), and the low-water mark that triggers a zero-length credit
/// top-up UIH back to the peer.
const CLIENT_INITIAL_CREDITS: u8 = 7;
const CLIENT_CREDIT_REFILL_THRESHOLD: u16 = 3;

impl RfcommClientState {
    /// Construct a client state targeting `server_channel` (e.g. the
    /// channel discovered via SDP for PBAP PSE). The DLCI is computed
    /// with the initiator-direction bit set, since we are the
    /// multiplexer initiator.
    pub fn new(server_channel: u8) -> Self {
        Self {
            server_channel,
            target_dlci: server_channel_dlci(server_channel, true),
            max_frame_size: RFCOMM_DEFAULT_MAX_FRAME_SIZE,
            phase: RfcommClientPhase::Idle,
            msc_local_pending: false,
            msc_remote_received: false,
            pending_events: Vec::new(),
            credit_flow: false,
            tx_credits: 0,
            rx_credits_outstanding: 0,
            secondary_dlcis: BTreeMap::new(),
            client_events: Vec::new(),
        }
    }

    /// True once the multiplexer SABM/UA has settled and the session is
    /// still alive — the precondition for attaching secondary DLCIs.
    /// (Any phase past `AwaitingMultiplexerUa` implies the mux is up;
    /// Closed/Failed mean the whole session is gone.)
    pub fn mux_is_open(&self) -> bool {
        !matches!(
            self.phase,
            RfcommClientPhase::Idle
                | RfcommClientPhase::AwaitingMultiplexerUa
                | RfcommClientPhase::Closed
                | RfcommClientPhase::Failed(_)
        )
    }

    /// Attach a new outbound DLCI on this initiator multiplexer (the
    /// twin of `RfcommState::attach_client_dlci` for sessions WE
    /// initiated — HARD-001 outbound reconnects). Returns the target
    /// DLCI and the PN command framed as a UIH-on-DLCI-0, ready for
    /// L2CAP wrapping. Wait for `ClientDlciEvent::Opened` before
    /// sending OBEX via `build_uih_on_client_dlci`.
    pub fn attach_client_dlci(&mut self, server_channel: u8) -> Result<(u8, Vec<u8>), String> {
        if !self.mux_is_open() {
            return Err(format!(
                "attach_client_dlci: initiator mux not open (phase {:?})",
                self.phase
            ));
        }
        // Same D-bit as the primary target: we initiated the mux, so a
        // remote server channel maps to DLCI = 2*scn + 1 (this is the
        // case where the spec rule and Bluedroid's empirical behaviour
        // agree — see RfcommState::attach_client_dlci for the shared-
        // inbound-mux divergence story).
        let target_dlci = server_channel_dlci(server_channel, true);
        if target_dlci == self.target_dlci || self.secondary_dlcis.contains_key(&target_dlci) {
            return Err(format!(
                "attach_client_dlci: dlci {} already in use",
                target_dlci
            ));
        }
        self.secondary_dlcis.insert(
            target_dlci,
            SecondaryDlci {
                phase: ClientDlciPhase::AwaitingPnRsp,
                msc_local_pending: false,
                msc_remote_received: false,
                credit_flow: false,
                tx_credits: 0,
                rx_credits_outstanding: u16::from(CLIENT_INITIAL_CREDITS),
            },
        );
        // CFC PN frame type (0xe0): attaching to an ALREADY-RUNNING mux
        // is the case where Bluedroid acks a legacy 0xf0 PN and then
        // silently drops the SABM that follows (proven on the inbound
        // shared mux) — declare credit-based flow control up front.
        let pn = build_parameter_negotiation_command_cfc(
            target_dlci,
            7,
            self.max_frame_size,
            CLIENT_INITIAL_CREDITS,
        );
        eprintln!(
            "[AokieRadio] RFCOMM initiator-mux DLCI {} attached (server channel {})",
            target_dlci, server_channel
        );
        // C/R = 1: we are the mux initiator issuing a command.
        Ok((
            target_dlci,
            build_uih(RFCOMM_DLCI_MULTIPLEXER, true, None, &pn),
        ))
    }

    /// Drain pending secondary-DLCI events whose `dlci` matches; events
    /// for other DLCIs stay queued for their own runtime — identical
    /// contract to `RfcommState::take_client_events_for_dlci`.
    pub fn take_client_events_for_dlci(&mut self, dlci: u8) -> Vec<ClientDlciEvent> {
        let all = std::mem::take(&mut self.client_events);
        let mut taken = Vec::new();
        let mut remaining = Vec::new();
        for ev in all {
            let ev_dlci = match &ev {
                ClientDlciEvent::Opened { dlci } => *dlci,
                ClientDlciEvent::Failed { dlci, .. } => *dlci,
                ClientDlciEvent::Payload { dlci, .. } => *dlci,
            };
            if ev_dlci == dlci {
                taken.push(ev);
            } else {
                remaining.push(ev);
            }
        }
        self.client_events = remaining;
        taken
    }

    /// Build a UIH frame on a secondary `dlci` carrying `payload`
    /// (OBEX bytes). Errors unless the DLCI reached `Open`. Consumes
    /// one transmit credit under credit-based flow control.
    pub fn build_uih_on_client_dlci(
        &mut self,
        dlci: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, String> {
        let client = self
            .secondary_dlcis
            .get_mut(&dlci)
            .ok_or_else(|| format!("build_uih_on_client_dlci: dlci {} not tracked", dlci))?;
        if !matches!(client.phase, ClientDlciPhase::Open) {
            return Err(format!(
                "build_uih_on_client_dlci: dlci {} not Open (phase {:?})",
                dlci, client.phase
            ));
        }
        if client.credit_flow {
            client.tx_credits = client.tx_credits.saturating_sub(1);
        }
        // C/R = 1: initiator-mux command (same rule as the primary DLCI).
        Ok(build_uih(dlci, true, None, payload))
    }

    /// Force-tear-down a secondary DLCI (MAS-stall recovery). Idempotent:
    /// emits the DISC even when the DLCI isn't tracked so half-open
    /// peer-side state can clear; tracking is dropped so a fresh
    /// `attach_client_dlci` can re-PN from scratch. Never touches the
    /// primary (HFP) DLCI — recovery must not kill the phone link.
    pub fn build_force_disc_secondary_dlci(&mut self, dlci: u8) -> Vec<u8> {
        self.secondary_dlcis.remove(&dlci);
        // C/R = 1: initiator command.
        build_disc(dlci, true)
    }

    /// False while credit-based flow control is active and the peer has
    /// granted us no transmit credits — data sent anyway would be a
    /// protocol violation the peer may silently discard. Callers with a
    /// command queue (the HFP client) hold the next frame until this
    /// turns true; the peer's credit grant arrives via `handle_packet`.
    pub fn can_send_data(&self) -> bool {
        !self.credit_flow || self.tx_credits > 0
    }

    pub fn server_channel(&self) -> u8 {
        self.server_channel
    }

    pub fn target_dlci(&self) -> u8 {
        self.target_dlci
    }

    pub fn max_frame_size(&self) -> u16 {
        self.max_frame_size
    }

    pub fn phase(&self) -> &RfcommClientPhase {
        &self.phase
    }

    pub fn is_open(&self) -> bool {
        matches!(self.phase, RfcommClientPhase::Open)
    }

    pub fn take_events(&mut self) -> Vec<RfcommClientEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Originate the multiplexer SABM. Call this once after the L2CAP
    /// channel to PSM_RFCOMM transitions to `Open`. Returns the SABM
    /// frame for the runtime to wrap in a basic frame + ACL packet.
    pub fn kickoff(&mut self) -> Result<Vec<u8>, String> {
        if !matches!(self.phase, RfcommClientPhase::Idle) {
            return Err(format!(
                "RFCOMM client kickoff in phase {:?}, expected Idle",
                self.phase
            ));
        }
        self.phase = RfcommClientPhase::AwaitingMultiplexerUa;
        Ok(build_sabm(RFCOMM_DLCI_MULTIPLEXER, true))
    }

    /// Drive the state machine with an inbound RFCOMM frame (already
    /// extracted from the L2CAP basic-frame layer). Returns zero or
    /// more frames the caller should send back. Side effects (Opened
    /// / Payload / Closed / Failed) are observable via `take_events`.
    pub fn handle_packet(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let frame = parse_frame(packet)?;
        eprintln!(
            "[AokieRadio] RFCOMM client frame in: kind={:?} dlci={} target={} payload={}B (phase {:?})",
            frame.kind,
            frame.dlci,
            self.target_dlci,
            frame.payload.len(),
            self.phase
        );
        match (frame.kind, frame.dlci) {
            (RfcommFrameKind::Ua, RFCOMM_DLCI_MULTIPLEXER) => self.on_ua_multiplexer(),
            (RfcommFrameKind::Ua, dlci) if dlci == self.target_dlci => self.on_ua_target(),
            // Secondary-DLCI arms come BEFORE the session-wide Dm/Disc
            // catch-alls: a refused/closed MAS or PBAP DLCI must fail
            // only ITSELF — never the HFP session sharing the mux.
            (RfcommFrameKind::Ua, dlci) if self.secondary_dlcis.contains_key(&dlci) => {
                self.on_ua_secondary(dlci)
            }
            (RfcommFrameKind::Dm, dlci) if self.secondary_dlcis.contains_key(&dlci) => {
                self.fail_secondary(dlci, format!("RFCOMM peer sent DM on dlci {}", dlci));
                Ok(Vec::new())
            }
            (RfcommFrameKind::Disc, dlci) if self.secondary_dlcis.contains_key(&dlci) => {
                self.fail_secondary(dlci, format!("RFCOMM peer sent DISC on dlci {}", dlci));
                Ok(vec![build_ua(dlci, false)])
            }
            // Session-fatal Dm/Disc ONLY for the mux itself or our primary
            // target DLCI. A stray Dm/Disc for any OTHER dlci (e.g. the
            // phone answering DM to a stall-recovery DISC of an MNS dlci
            // that was never open here) must never kill the phone link —
            // live 2026-07-13: exactly that DM tore down a healthy HFP
            // session mid-conversation.
            (RfcommFrameKind::Dm, dlci)
                if dlci == RFCOMM_DLCI_MULTIPLEXER || dlci == self.target_dlci =>
            {
                // DM = "go away". Treat as a hard fail so the caller
                // tears the L2CAP channel down. Don't auto-recover —
                // PBAP/MAP refusals are usually permanent (bond denied,
                // PIN refused) and retrying just pesters the user.
                self.fail(format!("RFCOMM peer sent DM on dlci {}", dlci));
                Ok(Vec::new())
            }
            (RfcommFrameKind::Disc, dlci)
                if dlci == RFCOMM_DLCI_MULTIPLEXER || dlci == self.target_dlci =>
            {
                self.phase = RfcommClientPhase::Closed;
                self.pending_events.push(RfcommClientEvent::Closed);
                Ok(vec![build_ua(dlci, false)])
            }
            (RfcommFrameKind::Dm, dlci) => {
                eprintln!(
                    "[AokieRadio] RFCOMM client ignoring DM for untracked dlci {} (session unaffected)",
                    dlci
                );
                Ok(Vec::new())
            }
            (RfcommFrameKind::Disc, dlci) => {
                // RFCOMM: DISC for a DLCI that isn't open answers DM.
                eprintln!(
                    "[AokieRadio] RFCOMM client answering DM to DISC for untracked dlci {} (session unaffected)",
                    dlci
                );
                Ok(vec![build_dm(dlci, false)])
            }
            (RfcommFrameKind::Uih, RFCOMM_DLCI_MULTIPLEXER) => self.on_mux_uih(frame.payload),
            (RfcommFrameKind::Uih, dlci) if dlci == self.target_dlci => {
                let mut out = Vec::new();
                // Credit byte (UIH with P/F=1) — the peer topping up our
                // transmit budget. Zero-length credit frames are how
                // Bluedroid delivers the REAL initial grant right after
                // the channel opens (its PN response says 0).
                if let Some(granted) = frame.credits {
                    if granted > 0 {
                        self.tx_credits = self.tx_credits.saturating_add(u16::from(granted));
                        eprintln!(
                            "[AokieRadio] RFCOMM client dlci {} credit grant +{} (tx_credits now {})",
                            dlci, granted, self.tx_credits
                        );
                    }
                }
                if !frame.payload.is_empty() {
                    if self.is_open() {
                        self.pending_events
                            .push(RfcommClientEvent::Payload(frame.payload.to_vec()));
                    } else {
                        eprintln!(
                            "[AokieRadio] RFCOMM client UIH on dlci {} ignored — channel not yet Open (phase {:?})",
                            dlci, self.phase
                        );
                    }
                    // Each data frame consumes one of the credits we
                    // granted; refill BEFORE the peer runs dry — an AG
                    // mid-SLC (or a PSE streaming a phonebook) that hits
                    // zero credits simply stops talking, which is
                    // exactly the stall we're preventing.
                    if self.credit_flow {
                        self.rx_credits_outstanding = self.rx_credits_outstanding.saturating_sub(1);
                        if self.rx_credits_outstanding <= CLIENT_CREDIT_REFILL_THRESHOLD {
                            let refill =
                                u16::from(CLIENT_INITIAL_CREDITS) - self.rx_credits_outstanding;
                            self.rx_credits_outstanding += refill;
                            out.push(build_uih(self.target_dlci, true, Some(refill as u8), &[]));
                        }
                    }
                }
                Ok(out)
            }
            (RfcommFrameKind::Uih, dlci) if self.secondary_dlcis.contains_key(&dlci) => {
                Ok(self.on_uih_secondary(dlci, frame.credits, frame.payload))
            }
            _ => {
                eprintln!(
                    "[AokieRadio] RFCOMM client ignored frame kind {:?} on dlci {}",
                    frame.kind, frame.dlci
                );
                Ok(Vec::new())
            }
        }
    }

    fn on_ua_secondary(&mut self, dlci: u8) -> Result<Vec<Vec<u8>>, String> {
        let Some(client) = self.secondary_dlcis.get_mut(&dlci) else {
            return Ok(Vec::new());
        };
        if !matches!(client.phase, ClientDlciPhase::AwaitingTargetUa) {
            return Ok(Vec::new());
        }
        // SABM acked — MSC exchange next, both directions, before the
        // DLCI counts as Open (same conservative walk as the primary).
        client.phase = ClientDlciPhase::AwaitingMscExchange;
        client.msc_local_pending = true;
        let msc = build_modem_status_command(dlci, RFCOMM_LOCAL_MODEM_STATUS);
        Ok(vec![build_uih(RFCOMM_DLCI_MULTIPLEXER, true, None, &msc)])
    }

    fn on_uih_secondary(&mut self, dlci: u8, credits: Option<u8>, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some(client) = self.secondary_dlcis.get_mut(&dlci) else {
            return out;
        };
        if let Some(granted) = credits {
            if granted > 0 {
                client.tx_credits = client.tx_credits.saturating_add(u16::from(granted));
                eprintln!(
                    "[AokieRadio] RFCOMM initiator-mux dlci {} credit grant +{} (tx_credits now {})",
                    dlci, granted, client.tx_credits
                );
            }
        }
        if !payload.is_empty() {
            self.client_events.push(ClientDlciEvent::Payload {
                dlci,
                payload: payload.to_vec(),
            });
            // Refill the peer's budget before it runs dry — a PSE/MSE
            // at zero credits simply stops talking mid-listing.
            if let Some(client) = self.secondary_dlcis.get_mut(&dlci) {
                if client.credit_flow {
                    client.rx_credits_outstanding = client.rx_credits_outstanding.saturating_sub(1);
                    if client.rx_credits_outstanding <= CLIENT_CREDIT_REFILL_THRESHOLD {
                        let refill =
                            u16::from(CLIENT_INITIAL_CREDITS) - client.rx_credits_outstanding;
                        client.rx_credits_outstanding += refill;
                        out.push(build_uih(dlci, true, Some(refill as u8), &[]));
                    }
                }
            }
        }
        out
    }

    fn maybe_open_secondary(&mut self, dlci: u8) {
        let Some(client) = self.secondary_dlcis.get_mut(&dlci) else {
            return;
        };
        if matches!(client.phase, ClientDlciPhase::AwaitingMscExchange)
            && !client.msc_local_pending
            && client.msc_remote_received
        {
            client.phase = ClientDlciPhase::Open;
            eprintln!("[AokieRadio] RFCOMM initiator-mux dlci {} open", dlci);
            self.client_events.push(ClientDlciEvent::Opened { dlci });
        }
    }

    fn fail_secondary(&mut self, dlci: u8, reason: String) {
        // REMOVE the tracking entry rather than tombstoning it: a peer
        // DM/DISC means the DLCI is dead on their side, and the next
        // MAP/PBAP attempt must be able to re-PN from scratch (live
        // 2026-07-13: a Failed tombstone wedged every subsequent
        // PollInbox with 'dlci 11 already in use'). Stray late frames
        // for the removed DLCI fall into the ignore arm harmlessly.
        self.secondary_dlcis.remove(&dlci);
        eprintln!(
            "[AokieRadio] RFCOMM initiator-mux dlci {} failed: {}",
            dlci, reason
        );
        self.client_events
            .push(ClientDlciEvent::Failed { dlci, reason });
    }

    fn on_ua_multiplexer(&mut self) -> Result<Vec<Vec<u8>>, String> {
        if !matches!(self.phase, RfcommClientPhase::AwaitingMultiplexerUa) {
            return Ok(Vec::new());
        }
        // Multiplexer up. Send PN command for our target DLCI before
        // SABM-ing the channel — most AGs (Pixel/iPhone/etc.) require
        // PN first or they reply DM to the SABM. priority 7 matches
        // typical PBAP/MAP values. The 0xf0 frame type is the spec's
        // CFC *request* (live-proven SABM-compatible on the own-mux
        // path), and the credits byte is the peer's INITIAL transmit
        // budget toward us: it MUST be non-zero — granting 0 left the
        // Bluedroid AG unable to send +BRSF during the HARD-001
        // outbound SLC, so it sat mute for 5s and DISC'd the channel.
        self.phase = RfcommClientPhase::AwaitingPnResponse;
        self.rx_credits_outstanding = u16::from(CLIENT_INITIAL_CREDITS);
        let pn = build_parameter_negotiation_command(
            self.target_dlci,
            7,
            self.max_frame_size,
            CLIENT_INITIAL_CREDITS,
        );
        Ok(vec![build_uih(RFCOMM_DLCI_MULTIPLEXER, true, None, &pn)])
    }

    fn on_ua_target(&mut self) -> Result<Vec<Vec<u8>>, String> {
        if !matches!(self.phase, RfcommClientPhase::AwaitingTargetUa) {
            return Ok(Vec::new());
        }
        // Target channel up. Send our MSC CMD and wait for both peer's
        // MSC RSP and the peer's own MSC CMD before declaring Opened.
        self.msc_local_pending = true;
        self.msc_remote_received = false;
        let msc = build_modem_status_command(self.target_dlci, RFCOMM_LOCAL_MODEM_STATUS);
        Ok(vec![build_uih(RFCOMM_DLCI_MULTIPLEXER, true, None, &msc)])
    }

    fn on_mux_uih(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        let command = parse_multiplexer_command(payload)?;
        match command {
            RfcommMuxCommand::ParameterNegotiation {
                is_response: true,
                dlci,
                frame_type,
                max_frame_size,
                credits,
                ..
            } if dlci == self.target_dlci
                && matches!(self.phase, RfcommClientPhase::AwaitingPnResponse) =>
            {
                // PN response settled — clamp the negotiated frame size
                // to our supported window and SABM the target DLCI.
                // Frame type 0xe0 = the peer ACCEPTED credit-based flow
                // control (Bluedroid always does); its credits byte is
                // our initial transmit budget — usually 0, with the
                // real grant arriving as a credit UIH after the channel
                // opens, so data senders must gate on `can_send_data`.
                self.max_frame_size =
                    max_frame_size.clamp(RFCOMM_MIN_MAX_FRAME_SIZE, RFCOMM_DEFAULT_MAX_FRAME_SIZE);
                self.credit_flow = frame_type == RFCOMM_PN_BLUETOOTH_FRAME_TYPE_CFC;
                self.tx_credits = u16::from(credits);
                eprintln!(
                    "[AokieRadio] RFCOMM client PN response: frame_type=0x{:02x} cfc={} initial_tx_credits={}",
                    frame_type, self.credit_flow, self.tx_credits
                );
                self.phase = RfcommClientPhase::AwaitingTargetUa;
                Ok(vec![build_sabm(self.target_dlci, true)])
            }
            RfcommMuxCommand::ModemStatus {
                is_response: true,
                dlci,
                ..
            } if dlci == self.target_dlci => {
                self.msc_local_pending = false;
                self.maybe_emit_opened();
                Ok(Vec::new())
            }
            RfcommMuxCommand::ModemStatus {
                is_response: false,
                dlci,
                signals,
            } if dlci == self.target_dlci => {
                self.msc_remote_received = true;
                self.maybe_emit_opened();
                let rsp = build_modem_status_response(dlci, signals);
                Ok(vec![build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &rsp)])
            }
            // ── Secondary-DLCI mux commands (MAP/PBAP on this mux) ──
            RfcommMuxCommand::ParameterNegotiation {
                is_response: true,
                dlci,
                frame_type,
                credits,
                ..
            } if self
                .secondary_dlcis
                .get(&dlci)
                .is_some_and(|c| matches!(c.phase, ClientDlciPhase::AwaitingPnRsp)) =>
            {
                let client = self.secondary_dlcis.get_mut(&dlci).expect("guard checked");
                client.credit_flow = frame_type == RFCOMM_PN_BLUETOOTH_FRAME_TYPE_CFC;
                client.tx_credits = u16::from(credits);
                client.phase = ClientDlciPhase::AwaitingTargetUa;
                eprintln!(
                    "[AokieRadio] RFCOMM initiator-mux dlci {} PN response: cfc={} initial_tx_credits={}",
                    dlci, client.credit_flow, client.tx_credits
                );
                Ok(vec![build_sabm(dlci, true)])
            }
            RfcommMuxCommand::ModemStatus {
                is_response: true,
                dlci,
                ..
            } if self.secondary_dlcis.contains_key(&dlci) => {
                if let Some(client) = self.secondary_dlcis.get_mut(&dlci) {
                    client.msc_local_pending = false;
                }
                self.maybe_open_secondary(dlci);
                Ok(Vec::new())
            }
            RfcommMuxCommand::ModemStatus {
                is_response: false,
                dlci,
                signals,
            } if self.secondary_dlcis.contains_key(&dlci) => {
                if let Some(client) = self.secondary_dlcis.get_mut(&dlci) {
                    client.msc_remote_received = true;
                }
                self.maybe_open_secondary(dlci);
                let rsp = build_modem_status_response(dlci, signals);
                Ok(vec![build_uih(RFCOMM_DLCI_MULTIPLEXER, false, None, &rsp)])
            }
            _ => {
                eprintln!(
                    "[AokieRadio] RFCOMM client ignored mux command (phase {:?})",
                    self.phase
                );
                Ok(Vec::new())
            }
        }
    }

    fn maybe_emit_opened(&mut self) {
        if !self.msc_local_pending
            && self.msc_remote_received
            && !matches!(self.phase, RfcommClientPhase::Open)
        {
            self.phase = RfcommClientPhase::Open;
            self.pending_events.push(RfcommClientEvent::Opened);
        }
    }

    /// Wrap an upper-layer payload (e.g. an OBEX request) in a UIH
    /// frame on the negotiated DLCI. Errors if the channel isn't Open
    /// — callers must wait for `RfcommClientEvent::Opened` first.
    /// Consumes one transmit credit under credit-based flow control;
    /// queue-driven callers should check `can_send_data()` first (a
    /// frame built at zero credits is sent anyway — some peers
    /// tolerate the violation, but Bluedroid may discard it).
    pub fn build_outbound_uih(&mut self, payload: &[u8]) -> Result<Vec<u8>, String> {
        if !self.is_open() {
            return Err(format!(
                "RFCOMM client UIH attempted in phase {:?}",
                self.phase
            ));
        }
        if self.credit_flow {
            self.tx_credits = self.tx_credits.saturating_sub(1);
        }
        // CR=true: we are the multiplexer initiator; commands from the
        // initiator carry C/R = 1.
        Ok(build_uih(self.target_dlci, true, None, payload))
    }

    /// Build a courtesy DISC on the target DLCI. Caller is expected to
    /// follow up with an L2CAP DisconnectionRequest after a short
    /// timeout if the peer doesn't reply UA.
    pub fn build_target_disc(&self) -> Vec<u8> {
        build_disc(self.target_dlci, true)
    }

    fn fail(&mut self, reason: String) {
        self.phase = RfcommClientPhase::Failed(reason.clone());
        self.pending_events.push(RfcommClientEvent::Failed(reason));
    }
}

fn describe_at_command(command: &hfp::HfpAtCommand) -> &'static str {
    match command {
        hfp::HfpAtCommand::SupportedFeatures { .. } => "AT+BRSF",
        hfp::HfpAtCommand::AvailableCodecs { .. } => "AT+BAC",
        hfp::HfpAtCommand::RetrieveIndicators => "AT+CIND=?",
        hfp::HfpAtCommand::RetrieveIndicatorStatus => "AT+CIND?",
        hfp::HfpAtCommand::RetrieveCallHoldSupport => "AT+CHLD=?",
        hfp::HfpAtCommand::EnableCallWaitingNotifications => "AT+CCWA",
        hfp::HfpAtCommand::CallHold(_) => "AT+CHLD",
        hfp::HfpAtCommand::ActivateClip(_) => "AT+CLIP",
        hfp::HfpAtCommand::EnableIndicatorUpdates(_) => "AT+CMER",
        hfp::HfpAtCommand::EnableAllIndicatorStatusUpdates(_) => "AT+BIA",
        hfp::HfpAtCommand::DisableNoiseReduction => "AT+NREC",
        hfp::HfpAtCommand::Answer => "ATA",
        hfp::HfpAtCommand::RejectOrHangup => "AT+CHUP",
        hfp::HfpAtCommand::Dial(_) => "ATD",
        hfp::HfpAtCommand::ConfirmCodec(_) => "AT+BCS",
        hfp::HfpAtCommand::ListCurrentCalls => "AT+CLCC",
    }
}

pub fn server_channel_dlci(server_channel: u8, initiator_direction: bool) -> u8 {
    ((server_channel & 0x1f) << 1) | u8::from(initiator_direction)
}

pub fn aokie_hfp_dlci() -> u8 {
    // We are the HFP HF — i.e. the *server* side that the phone (AG)
    // connects into. Per RFCOMM Bluetooth profile: for an incoming
    // server-channel connection, the DLCI direction bit equals
    // `multiplexer->outgoing` of the receiving side. The AG is the
    // multiplexer initiator, so our `outgoing = 0`, which gives
    // `dlci = (server_channel << 1) | 0 = 2` for channel 1. We
    // previously hardcoded the D-bit to 1, producing DLCI 3, which
    // doesn't match what the AG sends in its PN command — the AG
    // would target DLCI 2 (its own perspective: it's the initiator,
    // so its outgoing=1, yet for our incoming channel that flips to
    // 0). See BTstack `rfcomm.c` ~L495 for the same calculation.
    server_channel_dlci(AOKIE_RFCOMM_CHANNEL, false)
}

pub fn parse_frame(packet: &[u8]) -> Result<RfcommFrame<'_>, String> {
    require_len(packet, 4, "RFCOMM frame")?;
    let address = packet[0];
    if (address & 0x01) == 0 {
        return Err("RFCOMM address EA bit is not set".to_string());
    }

    let command_response = (address & 0x02) != 0;
    let dlci = address >> 2;
    let control = packet[1];
    let kind = frame_kind(control);
    let poll_final = (control & RFCOMM_CONTROL_POLL_FINAL) != 0;

    let (payload_len, length_octets) = decode_length(packet)?;
    let credits_offset = matches!(kind, RfcommFrameKind::Uih) && poll_final;
    let payload_offset = 2 + length_octets + usize::from(credits_offset);
    let fcs_offset = payload_offset + payload_len;
    require_len(packet, fcs_offset + 1, "RFCOMM frame payload")?;
    if packet.len() != fcs_offset + 1 {
        return Err(format!(
            "RFCOMM frame has {} trailing bytes",
            packet.len() - (fcs_offset + 1)
        ));
    }

    let fcs_input_len = if matches!(kind, RfcommFrameKind::Uih) {
        2
    } else {
        2 + length_octets
    };
    let expected_fcs = rfcomm_fcs(&packet[..fcs_input_len]);
    if packet[fcs_offset] != expected_fcs {
        return Err(format!(
            "RFCOMM FCS mismatch: expected 0x{:02x}, got 0x{:02x}",
            expected_fcs, packet[fcs_offset]
        ));
    }

    Ok(RfcommFrame {
        dlci,
        command_response,
        kind,
        poll_final,
        credits: if credits_offset {
            Some(packet[2 + length_octets])
        } else {
            None
        },
        payload: &packet[payload_offset..fcs_offset],
    })
}

pub fn build_sabm(dlci: u8, command_response: bool) -> Vec<u8> {
    build_frame(dlci, command_response, RFCOMM_CONTROL_SABM, None, &[])
}

pub fn build_ua(dlci: u8, command_response: bool) -> Vec<u8> {
    build_frame(dlci, command_response, RFCOMM_CONTROL_UA, None, &[])
}

pub fn build_dm(dlci: u8, command_response: bool) -> Vec<u8> {
    build_frame(dlci, command_response, RFCOMM_CONTROL_DM_PF, None, &[])
}

pub fn build_disc(dlci: u8, command_response: bool) -> Vec<u8> {
    build_frame(dlci, command_response, RFCOMM_CONTROL_DISC, None, &[])
}

pub fn build_uih(dlci: u8, command_response: bool, credits: Option<u8>, payload: &[u8]) -> Vec<u8> {
    let control = if credits.is_some() {
        RFCOMM_CONTROL_UIH_PF
    } else {
        RFCOMM_CONTROL_UIH
    };
    build_frame(dlci, command_response, control, credits, payload)
}

pub fn parse_multiplexer_command(payload: &[u8]) -> Result<RfcommMuxCommand<'_>, String> {
    require_len(payload, 2, "RFCOMM multiplexer command")?;
    let command_type = payload[0];
    let (len, length_octets) = decode_multiplexer_length(&payload[1..])?;
    let command_payload_offset = 1 + length_octets;
    require_len(
        payload,
        command_payload_offset + len,
        "RFCOMM multiplexer command payload",
    )?;
    let command_payload = &payload[command_payload_offset..command_payload_offset + len];

    match command_type {
        RFCOMM_MUX_PN_CMD | RFCOMM_MUX_PN_RSP => {
            require_len(command_payload, 8, "RFCOMM PN command")?;
            Ok(RfcommMuxCommand::ParameterNegotiation {
                is_response: command_type == RFCOMM_MUX_PN_RSP,
                dlci: command_payload[0],
                frame_type: command_payload[1],
                priority: command_payload[2],
                max_frame_size: u16::from_le_bytes([command_payload[4], command_payload[5]]),
                credits: command_payload[7],
            })
        }
        RFCOMM_MUX_MSC_CMD | RFCOMM_MUX_MSC_RSP => {
            require_len(command_payload, 2, "RFCOMM MSC frame")?;
            Ok(RfcommMuxCommand::ModemStatus {
                dlci: command_payload[0] >> 2,
                signals: command_payload[1],
                is_response: command_type == RFCOMM_MUX_MSC_RSP,
            })
        }
        RFCOMM_MUX_FCON_CMD => Ok(RfcommMuxCommand::FlowControlOn),
        RFCOMM_MUX_FCOFF_CMD => Ok(RfcommMuxCommand::FlowControlOff),
        _ => Ok(RfcommMuxCommand::Unknown {
            command_type,
            payload: command_payload,
        }),
    }
}

pub fn build_parameter_negotiation_command(
    dlci: u8,
    priority: u8,
    max_frame_size: u16,
    credits: u8,
) -> Vec<u8> {
    build_parameter_negotiation(
        RFCOMM_MUX_PN_CMD,
        dlci,
        RFCOMM_PN_BLUETOOTH_FRAME_TYPE,
        priority,
        max_frame_size,
        credits,
    )
}

/// Variant of `build_parameter_negotiation_command` that advertises
/// credit-based flow control (PN frame type 0xe0). Bluedroid (Pixel)
/// silently drops outbound MAS SABM unless this byte is set, even
/// though it answers PN with credits=0 in either case. Used by
/// `RfcommState::attach_client_dlci` for shared-mux DLCIs.
pub fn build_parameter_negotiation_command_cfc(
    dlci: u8,
    priority: u8,
    max_frame_size: u16,
    credits: u8,
) -> Vec<u8> {
    build_parameter_negotiation(
        RFCOMM_MUX_PN_CMD,
        dlci,
        RFCOMM_PN_BLUETOOTH_FRAME_TYPE_CFC,
        priority,
        max_frame_size,
        credits,
    )
}

pub fn build_parameter_negotiation_response(
    dlci: u8,
    priority: u8,
    max_frame_size: u16,
    credits: u8,
) -> Vec<u8> {
    build_parameter_negotiation(
        RFCOMM_MUX_PN_RSP,
        dlci,
        RFCOMM_PN_BLUETOOTH_FRAME_TYPE,
        priority,
        max_frame_size,
        credits,
    )
}

/// PN response declaring credit-based flow control ACCEPTED (frame type
/// 0xe0) — what Bluedroid actually answers our client PN with. The
/// credits byte is the initial transmit budget it grants us (usually 0,
/// with the real grant following as a credit UIH once the DLCI opens).
pub fn build_parameter_negotiation_response_cfc(
    dlci: u8,
    priority: u8,
    max_frame_size: u16,
    credits: u8,
) -> Vec<u8> {
    build_parameter_negotiation(
        RFCOMM_MUX_PN_RSP,
        dlci,
        RFCOMM_PN_BLUETOOTH_FRAME_TYPE_CFC,
        priority,
        max_frame_size,
        credits,
    )
}

pub fn build_modem_status_command(dlci: u8, signals: u8) -> Vec<u8> {
    build_modem_status(RFCOMM_MUX_MSC_CMD, dlci, signals)
}

pub fn build_modem_status_response(dlci: u8, signals: u8) -> Vec<u8> {
    build_modem_status(RFCOMM_MUX_MSC_RSP, dlci, signals)
}

fn build_frame(
    dlci: u8,
    command_response: bool,
    control: u8,
    credits: Option<u8>,
    payload: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len() + usize::from(credits.is_some()));
    out.push(address_byte(dlci, command_response));
    out.push(control);
    encode_length(payload.len(), &mut out);
    let fcs_input_len = if frame_kind(control) == RfcommFrameKind::Uih {
        2
    } else {
        out.len()
    };
    if let Some(credits) = credits {
        out.push(credits);
    }
    out.extend_from_slice(payload);
    out.push(rfcomm_fcs(&out[..fcs_input_len]));
    out
}

fn address_byte(dlci: u8, command_response: bool) -> u8 {
    0x01 | (u8::from(command_response) << 1) | ((dlci & 0x3f) << 2)
}

fn frame_kind(control: u8) -> RfcommFrameKind {
    match control & !RFCOMM_CONTROL_POLL_FINAL {
        0x2f => RfcommFrameKind::Sabm,
        0x63 => RfcommFrameKind::Ua,
        RFCOMM_CONTROL_DM => RfcommFrameKind::Dm,
        0x43 => RfcommFrameKind::Disc,
        RFCOMM_CONTROL_UIH => RfcommFrameKind::Uih,
        _ => RfcommFrameKind::Unknown(control),
    }
}

fn encode_length(len: usize, out: &mut Vec<u8>) {
    if len < 128 {
        out.push(((len as u8) << 1) | 1);
    } else {
        out.push(((len as u8) & 0x7f) << 1);
        out.push((len >> 7) as u8);
    }
}

fn decode_length(packet: &[u8]) -> Result<(usize, usize), String> {
    require_len(packet, 3, "RFCOMM length")?;
    decode_multiplexer_length(&packet[2..])
}

fn decode_multiplexer_length(payload: &[u8]) -> Result<(usize, usize), String> {
    require_len(payload, 1, "RFCOMM length")?;
    if (payload[0] & 0x01) != 0 {
        Ok(((payload[0] >> 1) as usize, 1))
    } else {
        require_len(payload, 2, "RFCOMM extended length")?;
        Ok((
            ((payload[0] >> 1) as usize) | ((payload[1] as usize) << 7),
            2,
        ))
    }
}

fn build_parameter_negotiation(
    command_type: u8,
    dlci: u8,
    frame_type: u8,
    priority: u8,
    max_frame_size: u16,
    credits: u8,
) -> Vec<u8> {
    build_multiplexer_command(
        command_type,
        &[
            dlci,
            frame_type,
            priority,
            0x00,
            max_frame_size as u8,
            (max_frame_size >> 8) as u8,
            0x00,
            credits,
        ],
    )
}

fn build_modem_status(command_type: u8, dlci: u8, signals: u8) -> Vec<u8> {
    build_multiplexer_command(command_type, &[address_byte(dlci, true), signals])
}

fn build_nsc_response(command_type: u8) -> Vec<u8> {
    build_multiplexer_command(RFCOMM_MUX_NSC_RSP, &[command_type])
}

fn build_empty_multiplexer_response(command_type: u8) -> Vec<u8> {
    build_multiplexer_command(command_type, &[])
}

fn build_multiplexer_command(command_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + payload.len());
    out.push(command_type);
    encode_length(payload.len(), &mut out);
    out.extend_from_slice(payload);
    out
}

fn rfcomm_fcs(data: &[u8]) -> u8 {
    let mut crc = RFCOMM_CRC8_INIT;
    for byte in data {
        crc ^= *byte;
        for _ in 0..8 {
            if (crc & 0x01) != 0 {
                crc = (crc >> 1) ^ RFCOMM_CRC8_POLY_REVERSED;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
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
    fn parses_and_builds_sabm_ua_dm_frames() {
        let sabm = build_sabm(RFCOMM_DLCI_MULTIPLEXER, true);
        assert_eq!(&sabm[..3], &[0x03, RFCOMM_CONTROL_SABM, 0x01]);
        let parsed = parse_frame(&sabm).unwrap();
        assert_eq!(parsed.dlci, RFCOMM_DLCI_MULTIPLEXER);
        assert_eq!(parsed.kind, RfcommFrameKind::Sabm);
        assert!(parsed.command_response);
        assert!(parsed.poll_final);

        let ua = build_ua(aokie_hfp_dlci(), true);
        assert_eq!(parse_frame(&ua).unwrap().kind, RfcommFrameKind::Ua);

        let dm = build_dm(aokie_hfp_dlci(), true);
        assert_eq!(parse_frame(&dm).unwrap().kind, RfcommFrameKind::Dm);
    }

    #[test]
    fn parses_and_builds_uih_frames_with_credits() {
        let frame = build_uih(aokie_hfp_dlci(), false, Some(7), b"AT\r");
        // DLCI 2 (server channel 1, D-bit=0 since AG is mux initiator):
        // address byte = EA(1) | CR(0) | (2<<2) = 0x09
        assert_eq!(frame[0], 0x09);
        assert_eq!(frame[1], RFCOMM_CONTROL_UIH_PF);
        assert_eq!(frame[2], 0x07);
        assert_eq!(frame[3], 7);

        let parsed = parse_frame(&frame).unwrap();
        assert_eq!(parsed.dlci, aokie_hfp_dlci());
        assert_eq!(parsed.kind, RfcommFrameKind::Uih);
        assert_eq!(parsed.credits, Some(7));
        assert_eq!(parsed.payload, b"AT\r");
    }

    #[test]
    fn rejects_bad_frame_check_sequence() {
        let mut frame = build_sabm(RFCOMM_DLCI_MULTIPLEXER, true);
        let last = frame.len() - 1;
        frame[last] ^= 0xff;
        assert!(parse_frame(&frame).is_err());
    }

    #[test]
    fn parses_multiplexer_commands() {
        let pn = build_parameter_negotiation_command(aokie_hfp_dlci(), 3, 256, 5);
        assert_eq!(
            parse_multiplexer_command(&pn).unwrap(),
            RfcommMuxCommand::ParameterNegotiation {
                is_response: false,
                dlci: aokie_hfp_dlci(),
                frame_type: RFCOMM_PN_BLUETOOTH_FRAME_TYPE,
                priority: 3,
                max_frame_size: 256,
                credits: 5,
            }
        );

        let pn_rsp = build_parameter_negotiation_response(aokie_hfp_dlci(), 3, 256, 5);
        assert_eq!(
            parse_multiplexer_command(&pn_rsp).unwrap(),
            RfcommMuxCommand::ParameterNegotiation {
                is_response: true,
                dlci: aokie_hfp_dlci(),
                frame_type: RFCOMM_PN_BLUETOOTH_FRAME_TYPE,
                priority: 3,
                max_frame_size: 256,
                credits: 5,
            }
        );

        let msc = build_modem_status_command(aokie_hfp_dlci(), 0x8d);
        assert_eq!(
            parse_multiplexer_command(&msc).unwrap(),
            RfcommMuxCommand::ModemStatus {
                dlci: aokie_hfp_dlci(),
                signals: 0x8d,
                is_response: false,
            }
        );
    }

    #[test]
    fn state_opens_multiplexer_and_hfp_channel() {
        let mut state = RfcommState::new();
        let responses = state
            .handle_packet(&build_sabm(RFCOMM_DLCI_MULTIPLEXER, true))
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            parse_frame(&responses[0]).unwrap().kind,
            RfcommFrameKind::Ua
        );
        assert!(state.multiplexer_open());

        let pn = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_parameter_negotiation_command(aokie_hfp_dlci(), 3, 256, 5),
        );
        let responses = state.handle_packet(&pn).unwrap();
        let response = parse_frame(&responses[0]).unwrap();
        assert_eq!(response.kind, RfcommFrameKind::Uih);
        assert_eq!(response.command_response, false);
        assert_eq!(response.payload[0], RFCOMM_MUX_PN_RSP);

        let responses = state
            .handle_packet(&build_sabm(aokie_hfp_dlci(), true))
            .unwrap();
        // SABM on HFP DLCI: we send UA and (per HFP §4.2.1) our MSC CMD.
        // The first SLC AT command must NOT go out yet — it waits for
        // the AG to ack our MSC and to send its own MSC CMD.
        assert_eq!(responses.len(), 2);
        assert_eq!(
            parse_frame(&responses[0]).unwrap().kind,
            RfcommFrameKind::Ua
        );
        let our_msc = parse_frame(&responses[1]).unwrap();
        assert_eq!(our_msc.kind, RfcommFrameKind::Uih);
        assert_eq!(our_msc.dlci, RFCOMM_DLCI_MULTIPLEXER);
        assert_eq!(our_msc.payload[0], RFCOMM_MUX_MSC_CMD);
        assert_eq!(state.hfp_channel_state(), RfcommChannelState::Open);

        // AG acks our MSC CMD with MSC RSP — half the exchange done.
        let msc_rsp_from_ag = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_modem_status_response(aokie_hfp_dlci(), RFCOMM_LOCAL_MODEM_STATUS),
        );
        let responses = state.handle_packet(&msc_rsp_from_ag).unwrap();
        // Just an MSC RSP — no replies, no AT yet (waiting on AG's MSC CMD).
        assert!(responses.is_empty());

        // AG sends its MSC CMD — now both directions have completed,
        // so we both reply MSC RSP AND fire the first SLC AT command.
        let msc_cmd_from_ag = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_modem_status_command(aokie_hfp_dlci(), RFCOMM_LOCAL_MODEM_STATUS),
        );
        let responses = state.handle_packet(&msc_cmd_from_ag).unwrap();
        assert_eq!(responses.len(), 2);
        let our_msc_rsp = parse_frame(&responses[0]).unwrap();
        assert_eq!(our_msc_rsp.dlci, RFCOMM_DLCI_MULTIPLEXER);
        assert_eq!(our_msc_rsp.payload[0], RFCOMM_MUX_MSC_RSP);
        let first_hfp_command = parse_frame(&responses[1]).unwrap();
        assert_eq!(first_hfp_command.dlci, aokie_hfp_dlci());
        assert_eq!(first_hfp_command.payload, b"AT+BRSF=693\r");
    }

    #[test]
    fn state_clamps_pathological_pn_max_frame_size_to_minimum() {
        // A peer that sends max_frame_size=0 (or any value below the
        // RFCOMM minimum) used to slip through unchanged, leaving the
        // state machine vulnerable to divide-by-zero in the frame
        // builder. The clamp applies a sane floor so we keep talking.
        let mut state = RfcommState::new();
        let _ = state
            .handle_packet(&build_sabm(RFCOMM_DLCI_MULTIPLEXER, true))
            .unwrap();
        let pn = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_parameter_negotiation_command(aokie_hfp_dlci(), 3, 0, 5),
        );
        let _ = state.handle_packet(&pn).unwrap();
        assert_eq!(state.max_frame_size, RFCOMM_MIN_MAX_FRAME_SIZE);

        // A reasonable value in the middle of the range survives.
        let pn = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_parameter_negotiation_command(aokie_hfp_dlci(), 3, 64, 5),
        );
        let _ = state.handle_packet(&pn).unwrap();
        assert_eq!(state.max_frame_size, 64);

        // And one above the maximum is capped.
        let pn = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_parameter_negotiation_command(aokie_hfp_dlci(), 3, 1024, 5),
        );
        let _ = state.handle_packet(&pn).unwrap();
        assert_eq!(state.max_frame_size, RFCOMM_DEFAULT_MAX_FRAME_SIZE);
    }

    #[test]
    fn state_sequences_hfp_service_level_commands_on_ok() {
        // Default (holdAndCallWaiting off): AT+CHLD=? and AT+CCWA=1 are
        // SKIPPED — we don't advertise three-way in BRSF, so probing the
        // AG's hold support would be out of spec (Phase 4 review, bug #1).
        let mut state = open_hfp_state();
        let expected = [
            b"AT+BAC=1,2\r".as_slice(),
            b"AT+CIND=?\r".as_slice(),
            b"AT+CIND?\r".as_slice(),
            b"AT+CLIP=1\r".as_slice(),
            b"AT+CMER=3,0,0,1\r".as_slice(),
            b"AT+BIA=1,1,1,1,1,1,1\r".as_slice(),
            b"AT+NREC=0\r".as_slice(),
        ];

        for command in expected {
            let responses = state
                .handle_packet(&build_uih(aokie_hfp_dlci(), false, None, b"\r\nOK\r\n"))
                .unwrap();
            assert_eq!(responses.len(), 1);
            assert_eq!(parse_frame(&responses[0]).unwrap().payload, command);
        }

        // The bare-OK run above never carried a +CIND DEFINITIONS line, so
        // the state re-requests it ONCE before readiness (phantom-answer
        // hardening: ringing on default indices misreads as answered).
        let responses = state
            .handle_packet(&build_uih(aokie_hfp_dlci(), false, None, b"\r\nOK\r\n"))
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(parse_frame(&responses[0]).unwrap().payload, b"AT+CIND=?\r");
        let responses = state
            .handle_packet(&build_uih(
                aokie_hfp_dlci(),
                false,
                None,
                b"\r\n+CIND: (\"call\",(0,1)),(\"callsetup\",(0-3))\r\nOK\r\n",
            ))
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(parse_frame(&responses[0]).unwrap().payload, b"AT+CIND?\r");

        let responses = state
            .handle_packet(&build_uih(
                aokie_hfp_dlci(),
                false,
                None,
                b"\r\n+CIND: 0,0\r\nOK\r\n",
            ))
            .unwrap();
        assert!(responses.is_empty());
        assert!(state.hfp_state().service_level_ready());
        assert_eq!(
            state.take_hfp_events(),
            vec![hfp::HfpEvent::ServiceLevelConnectionReady]
        );
        // The definitions parsed on the retry own the mapping now.
        assert_eq!(state.hfp_state().call_indicator_index(), 1);
    }

    /// Phase 4: with holdAndCallWaiting on and an AG that advertises
    /// three-way calling, the SLC includes AT+CHLD=? + AT+CCWA=1, BRSF
    /// carries HF bit 1, and the capability gate negotiates.
    #[test]
    fn call_waiting_slc_probes_negotiate_when_both_sides_support_it() {
        let mut state = RfcommState::new();
        state.set_wbs_supported(true);
        state.set_call_waiting_enabled(true);
        let _ = state
            .handle_packet(&build_sabm(RFCOMM_DLCI_MULTIPLEXER, true))
            .unwrap();
        let _ = state
            .handle_packet(&build_sabm(aokie_hfp_dlci(), true))
            .unwrap();
        let msc_rsp = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_modem_status_response(aokie_hfp_dlci(), RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = state.handle_packet(&msc_rsp).unwrap();
        let msc_cmd = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_modem_status_command(aokie_hfp_dlci(), RFCOMM_LOCAL_MODEM_STATUS),
        );
        let out = state.handle_packet(&msc_cmd).unwrap();
        let brsf = out
            .iter()
            .filter_map(|f| parse_frame(f).ok())
            .find(|f| f.payload.starts_with(b"AT+BRSF"))
            .expect("BRSF fires at MSC completion");
        assert_eq!(brsf.payload, b"AT+BRSF=695\r", "HF bit 1 advertised");

        let mut sent = Vec::new();
        for reply in [
            "\r\n+BRSF: 4095\r\nOK\r\n", // AG advertises three-way (bit 0)
            "\r\nOK\r\n",                // AT+BAC
            "\r\n+CIND: (\"call\",(0,1)),(\"callsetup\",(0-3)),(\"callheld\",(0-2))\r\nOK\r\n",
            "\r\n+CIND: 0,0,0\r\nOK\r\n",
            "\r\n+CHLD: (0,1,1x,2,2x,3)\r\nOK\r\n",
            "\r\nOK\r\n", // AT+CCWA=1
            "\r\nOK\r\n", // AT+CLIP
            "\r\nOK\r\n", // AT+CMER
            "\r\nOK\r\n", // AT+BIA
            "\r\nOK\r\n", // AT+NREC
        ] {
            for frame in state
                .handle_packet(&build_uih(aokie_hfp_dlci(), false, None, reply.as_bytes()))
                .unwrap()
            {
                sent.push(parse_frame(&frame).unwrap().payload.to_vec());
            }
        }
        assert!(sent.iter().any(|p| p == b"AT+CHLD=?\r"), "CHLD probe sent");
        assert!(sent.iter().any(|p| p == b"AT+CCWA=1\r"), "CCWA armed");
        assert!(state.hfp_state().service_level_ready());
        assert!(state.hfp_state().three_way_negotiated());
    }

    /// Phase 4 review bug #1: an AG that answers a capability probe with
    /// ERROR must lose only the capability, never the HFP connection.
    #[test]
    fn call_waiting_probe_error_is_non_fatal_and_slc_still_readies() {
        let mut state = RfcommState::new();
        state.set_call_waiting_enabled(true);
        let _ = state
            .handle_packet(&build_sabm(RFCOMM_DLCI_MULTIPLEXER, true))
            .unwrap();
        let _ = state
            .handle_packet(&build_sabm(aokie_hfp_dlci(), true))
            .unwrap();
        let msc_rsp = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_modem_status_response(aokie_hfp_dlci(), RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = state.handle_packet(&msc_rsp).unwrap();
        let msc_cmd = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_modem_status_command(aokie_hfp_dlci(), RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = state.handle_packet(&msc_cmd).unwrap();

        let mut sent = Vec::new();
        for reply in [
            "\r\n+BRSF: 4095\r\nOK\r\n",
            "\r\nOK\r\n",
            "\r\n+CIND: (\"call\",(0,1)),(\"callsetup\",(0-3))\r\nOK\r\n",
            "\r\n+CIND: 0,0\r\nOK\r\n",
            "\r\nERROR\r\n", // AT+CHLD=? refused
            "\r\nERROR\r\n", // AT+CCWA=1 refused too
            "\r\nOK\r\n",    // AT+CLIP
            "\r\nOK\r\n",    // AT+CMER
            "\r\nOK\r\n",    // AT+BIA
            "\r\nOK\r\n",    // AT+NREC
        ] {
            for frame in state
                .handle_packet(&build_uih(aokie_hfp_dlci(), false, None, reply.as_bytes()))
                .unwrap()
            {
                sent.push(parse_frame(&frame).unwrap().payload.to_vec());
            }
        }
        assert!(sent.iter().any(|p| p == b"AT+CLIP=1\r"), "queue continued");
        assert!(state.hfp_state().service_level_ready(), "SLC survived");
        assert!(!state.hfp_state().three_way_negotiated(), "capability off");
        let events = state.take_hfp_events();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, hfp::HfpEvent::ServiceLevelConnectionFailed(_))),
            "no fatal SLC event for a refused capability probe: {events:?}"
        );
    }

    #[test]
    fn state_emits_slc_failure_on_ag_error_and_drains_pending_queue() {
        let mut state = open_hfp_state();
        // First two SLC commands return OK so the queue advances.
        let _ = state
            .handle_packet(&build_uih(aokie_hfp_dlci(), false, None, b"\r\nOK\r\n"))
            .unwrap();
        let _ = state
            .handle_packet(&build_uih(aokie_hfp_dlci(), false, None, b"\r\nOK\r\n"))
            .unwrap();

        // The third reply is ERROR — pre-fix this stalled forever.
        let responses = state
            .handle_packet(&build_uih(aokie_hfp_dlci(), false, None, b"\r\nERROR\r\n"))
            .unwrap();
        assert!(responses.is_empty());
        assert!(!state.hfp_state().service_level_ready());

        // Subsequent OKs must NOT resume the SLC queue or mark it ready.
        let responses = state
            .handle_packet(&build_uih(aokie_hfp_dlci(), false, None, b"\r\nOK\r\n"))
            .unwrap();
        assert!(responses.is_empty());
        assert!(!state.hfp_state().service_level_ready());

        let events = state.take_hfp_events();
        assert!(
            events.contains(&hfp::HfpEvent::ServiceLevelConnectionFailed(
                "AT+CIND=?".to_string()
            )),
            "expected SLC failure event tagged with the in-flight command, got {events:?}"
        );
    }

    #[test]
    fn state_parses_hfp_events_and_confirms_codec() {
        let mut state = open_hfp_state();
        let responses = state
            .handle_packet(&build_uih(
                aokie_hfp_dlci(),
                false,
                None,
                b"\r\nRING\r\n+CLIP: \"+15551234567\",145\r\n+CIEV: 2,1\r\n+BCS: 2\r\n",
            ))
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(parse_frame(&responses[0]).unwrap().payload, b"AT+BCS=2\r");
        assert_eq!(
            state.hfp_events(),
            &[
                hfp::HfpEvent::IncomingCall,
                hfp::HfpEvent::Ringing,
                hfp::HfpEvent::CallerId("+15551234567".to_string()),
                hfp::HfpEvent::CallAnswered,
                hfp::HfpEvent::CodecSelected {
                    codec: "mSBC".to_string(),
                    sample_rate: 16000,
                },
            ]
        );
    }

    #[test]
    fn state_builds_hfp_call_control_frames() {
        let mut state = open_hfp_state();
        let answer = state
            .build_call_control_command(hfp::HfpAtCommand::Answer)
            .unwrap();
        assert_eq!(parse_frame(&answer).unwrap().payload, b"ATA\r");

        let hangup = state
            .build_call_control_command(hfp::HfpAtCommand::RejectOrHangup)
            .unwrap();
        assert_eq!(parse_frame(&hangup).unwrap().payload, b"AT+CHUP\r");
    }

    #[test]
    fn slc_stall_watchdog_fails_when_ag_drops_reply() {
        // Drive the state into "AT+BRSF sent, awaiting OK". After
        // tick_hfp_stall fires past the timeout, the SLC must be marked
        // failed and an event surfaced — without this the queue stalls
        // forever when the AG drops a reply.
        let mut state = open_hfp_state();
        // Sanity: an SLC command is in flight after open_hfp_state.
        assert!(state.hfp_in_flight_command.is_some());
        // Forge a "sent 30 s ago" timestamp.
        state.hfp_in_flight_command_sent_at = Some(Instant::now() - Duration::from_secs(30));
        let now = Instant::now();
        let fired = state.tick_hfp_stall(now, Duration::from_secs(10));
        assert!(fired, "stall watchdog should fire when timeout exceeded");
        assert!(state.hfp_slc_failed);
        let events = state.take_hfp_events();
        assert!(events.iter().any(|e| matches!(
            e,
            hfp::HfpEvent::ServiceLevelConnectionFailed(reason) if reason.contains("timed out")
        )));
        // Subsequent ticks must not re-fire (in-flight command is taken).
        assert!(!state.tick_hfp_stall(now, Duration::from_secs(10)));
    }

    #[test]
    fn registered_extra_server_channel_routes_sabm_uih_disc() {
        use std::sync::Mutex as StdMutex;

        const FAKE_SERVER_CHANNEL: u8 = 4; // arbitrary non-HFP channel
        let fake_dlci = server_channel_dlci(FAKE_SERVER_CHANNEL, false);

        let captured: Arc<StdMutex<Vec<Vec<u8>>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_for_uih = captured.clone();
        let sabm_called: Arc<StdMutex<bool>> = Arc::new(StdMutex::new(false));
        let sabm_called_for_handler = sabm_called.clone();
        let disc_called: Arc<StdMutex<bool>> = Arc::new(StdMutex::new(false));
        let disc_called_for_handler = disc_called.clone();

        let mut state = RfcommState::new();
        let _ = state
            .handle_packet(&build_sabm(RFCOMM_DLCI_MULTIPLEXER, true))
            .unwrap();
        state.register_server_channel(
            FAKE_SERVER_CHANNEL,
            ServerChannelHandlers {
                on_sabm: Arc::new(move || {
                    *sabm_called_for_handler.lock().unwrap() = true;
                    vec![build_ua(
                        server_channel_dlci(FAKE_SERVER_CHANNEL, false),
                        true,
                    )]
                }),
                on_disc: Arc::new(move || {
                    *disc_called_for_handler.lock().unwrap() = true;
                    build_ua(server_channel_dlci(FAKE_SERVER_CHANNEL, false), true)
                }),
                on_uih: Arc::new(move |payload| {
                    captured_for_uih.lock().unwrap().push(payload.to_vec());
                    Ok(vec![vec![0xab, 0xcd]])
                }),
            },
        );

        // SABM on the fake channel — registry's on_sabm runs, peer gets UA.
        let resp = state.handle_packet(&build_sabm(fake_dlci, true)).unwrap();
        assert!(*sabm_called.lock().unwrap(), "on_sabm should have fired");
        assert_eq!(resp.len(), 1);
        assert_eq!(parse_frame(&resp[0]).unwrap().kind, RfcommFrameKind::Ua);

        // UIH payload on the fake channel — registry's on_uih runs.
        let resp = state
            .handle_packet(&build_uih(fake_dlci, true, None, b"\x01\x02\x03"))
            .unwrap();
        assert_eq!(*captured.lock().unwrap(), vec![b"\x01\x02\x03".to_vec()]);
        assert_eq!(resp, vec![vec![0xab, 0xcd]]);

        // DISC on the fake channel — registry's on_disc runs.
        let _ = state.handle_packet(&build_disc(fake_dlci, true)).unwrap();
        assert!(*disc_called.lock().unwrap(), "on_disc should have fired");

        // SABM on an UNregistered server channel still replies DM (the
        // negative case Phase 0c must preserve).
        let unregistered_dlci = server_channel_dlci(7, false);
        let resp = state
            .handle_packet(&build_sabm(unregistered_dlci, true))
            .unwrap();
        assert_eq!(resp.len(), 1);
        assert_eq!(parse_frame(&resp[0]).unwrap().kind, RfcommFrameKind::Dm);
    }

    #[test]
    fn force_disc_keeps_server_channel_handler_registered() {
        // Regression for the MAS-stall-recovery breakage: when the
        // recovery path force-DISCs MNS dlci 4, dropping the handler
        // would lock Pixel out — its subsequent PN to reopen MNS gets
        // an NSC and the link silently degrades to dead-air. The
        // handler must survive so the next SABM/PN routes through it.
        use std::sync::Mutex as StdMutex;

        const SERVER_CHANNEL: u8 = 4;
        let dlci_for_test = server_channel_dlci(SERVER_CHANNEL, false);
        let sabm_called: Arc<StdMutex<bool>> = Arc::new(StdMutex::new(false));
        let sabm_called_for_handler = sabm_called.clone();
        let dlci_for_sabm = dlci_for_test;
        let dlci_for_disc = dlci_for_test;

        let mut state = RfcommState::new();
        let _ = state
            .handle_packet(&build_sabm(RFCOMM_DLCI_MULTIPLEXER, true))
            .unwrap();
        state.register_server_channel(
            SERVER_CHANNEL,
            ServerChannelHandlers {
                on_sabm: Arc::new(move || {
                    *sabm_called_for_handler.lock().unwrap() = true;
                    vec![build_ua(dlci_for_sabm, true)]
                }),
                on_disc: Arc::new(move || build_ua(dlci_for_disc, true)),
                on_uih: Arc::new(|_| Ok(vec![])),
            },
        );

        // Force-DISC the registered server-channel DLCI.
        let _ = state.build_force_disc_dlci(dlci_for_test);

        // Peer's subsequent SABM (to reopen the channel) MUST still
        // route through the registered on_sabm closure — i.e. the
        // handler is preserved across force-DISC.
        let _ = state
            .handle_packet(&build_sabm(dlci_for_test, true))
            .unwrap();
        assert!(
            *sabm_called.lock().unwrap(),
            "on_sabm must fire after force-DISC; handler was incorrectly removed"
        );
    }

    #[test]
    fn slc_stall_watchdog_no_op_when_within_timeout() {
        let mut state = open_hfp_state();
        let now = Instant::now();
        // Timestamp set by next_hfp_command_frame is essentially "now",
        // so a 10 s timeout shouldn't trip immediately.
        assert!(!state.tick_hfp_stall(now, Duration::from_secs(10)));
        assert!(!state.hfp_slc_failed);
    }

    #[test]
    fn rfcomm_client_kickoff_emits_multiplexer_sabm() {
        let mut client = RfcommClientState::new(19); // PBAP PSE typical channel
        assert_eq!(client.target_dlci(), server_channel_dlci(19, true));
        assert!(matches!(client.phase(), RfcommClientPhase::Idle));

        let sabm = client.kickoff().unwrap();
        let parsed = parse_frame(&sabm).unwrap();
        assert_eq!(parsed.kind, RfcommFrameKind::Sabm);
        assert_eq!(parsed.dlci, RFCOMM_DLCI_MULTIPLEXER);
        assert!(parsed.command_response, "initiator's SABM has CR=1");
        assert!(matches!(
            client.phase(),
            RfcommClientPhase::AwaitingMultiplexerUa
        ));

        // Calling kickoff twice returns Err — would otherwise re-send
        // SABM and confuse the peer's mux state machine.
        assert!(client.kickoff().is_err());
    }

    #[test]
    fn rfcomm_client_walks_full_handshake_to_open() {
        let mut client = RfcommClientState::new(19);
        let _ = client.kickoff().unwrap();
        let target_dlci = client.target_dlci();

        // Peer replies UA on mux DLCI 0. We should emit a PN command.
        let ua_mux = build_ua(RFCOMM_DLCI_MULTIPLEXER, true);
        let responses = client.handle_packet(&ua_mux).unwrap();
        assert_eq!(responses.len(), 1);
        let pn_frame = parse_frame(&responses[0]).unwrap();
        assert_eq!(pn_frame.kind, RfcommFrameKind::Uih);
        assert_eq!(pn_frame.dlci, RFCOMM_DLCI_MULTIPLEXER);
        assert!(pn_frame.command_response, "initiator's PN CMD has CR=1");
        let pn_cmd = parse_multiplexer_command(pn_frame.payload).unwrap();
        assert_eq!(
            pn_cmd,
            RfcommMuxCommand::ParameterNegotiation {
                is_response: false,
                dlci: target_dlci,
                frame_type: RFCOMM_PN_BLUETOOTH_FRAME_TYPE,
                priority: 7,
                max_frame_size: RFCOMM_DEFAULT_MAX_FRAME_SIZE,
                // Initial credits we grant the peer — must be non-zero
                // or a CFC peer (Bluedroid AG) can never speak first.
                credits: 7,
            }
        );
        assert!(matches!(
            client.phase(),
            RfcommClientPhase::AwaitingPnResponse
        ));

        // Peer replies PN response. We expect SABM on the target DLCI.
        let pn_rsp = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            false,
            None,
            &build_parameter_negotiation_response(target_dlci, 7, 256, 7),
        );
        let responses = client.handle_packet(&pn_rsp).unwrap();
        assert_eq!(responses.len(), 1);
        let sabm = parse_frame(&responses[0]).unwrap();
        assert_eq!(sabm.kind, RfcommFrameKind::Sabm);
        assert_eq!(sabm.dlci, target_dlci);
        assert_eq!(client.max_frame_size(), 127); // clamped to default ceiling
        assert!(matches!(
            client.phase(),
            RfcommClientPhase::AwaitingTargetUa
        ));

        // Peer UA on target DLCI: we send our MSC CMD on mux DLCI 0.
        let ua_target = build_ua(target_dlci, true);
        let responses = client.handle_packet(&ua_target).unwrap();
        assert_eq!(responses.len(), 1);
        let msc_cmd = parse_frame(&responses[0]).unwrap();
        assert_eq!(msc_cmd.dlci, RFCOMM_DLCI_MULTIPLEXER);
        let parsed = parse_multiplexer_command(msc_cmd.payload).unwrap();
        assert_eq!(
            parsed,
            RfcommMuxCommand::ModemStatus {
                is_response: false,
                dlci: target_dlci,
                signals: RFCOMM_LOCAL_MODEM_STATUS,
            }
        );
        assert!(client.take_events().is_empty(), "Opened not yet");

        // Peer MSC RSP for our MSC CMD: half the exchange done.
        let msc_rsp = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            false,
            None,
            &build_modem_status_response(target_dlci, RFCOMM_LOCAL_MODEM_STATUS),
        );
        let responses = client.handle_packet(&msc_rsp).unwrap();
        assert!(responses.is_empty());
        assert!(
            client.take_events().is_empty(),
            "Opened blocked until peer's MSC CMD arrives"
        );

        // Peer's own MSC CMD: we reply MSC RSP and channel transitions to Open.
        let peer_msc_cmd = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            false,
            None,
            &build_modem_status_command(target_dlci, RFCOMM_LOCAL_MODEM_STATUS),
        );
        let responses = client.handle_packet(&peer_msc_cmd).unwrap();
        assert_eq!(responses.len(), 1);
        let our_msc_rsp = parse_frame(&responses[0]).unwrap();
        let parsed = parse_multiplexer_command(our_msc_rsp.payload).unwrap();
        assert_eq!(
            parsed,
            RfcommMuxCommand::ModemStatus {
                is_response: true,
                dlci: target_dlci,
                signals: RFCOMM_LOCAL_MODEM_STATUS,
            }
        );
        assert!(client.is_open());
        assert_eq!(client.take_events(), vec![RfcommClientEvent::Opened]);
    }

    #[test]
    fn rfcomm_client_msc_order_doesnt_matter_for_opened_event() {
        // Variant: peer sends its MSC CMD *before* MSC RSP. Some AGs do
        // this and we must still go Open after the second one arrives.
        let mut client = open_rfcomm_client();
        let target_dlci = client.target_dlci();

        let peer_msc_cmd = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            false,
            None,
            &build_modem_status_command(target_dlci, RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = client.handle_packet(&peer_msc_cmd).unwrap();
        assert!(!client.is_open(), "still waiting on our MSC RSP");

        let msc_rsp = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            false,
            None,
            &build_modem_status_response(target_dlci, RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = client.handle_packet(&msc_rsp).unwrap();
        assert!(client.is_open());
        assert_eq!(client.take_events(), vec![RfcommClientEvent::Opened]);
    }

    #[test]
    fn rfcomm_client_routes_uih_payload_after_open() {
        let mut client = open_rfcomm_client();
        finish_msc_exchange(&mut client);
        assert!(client.is_open());
        let _ = client.take_events();

        let payload = b"\xa0\x00\x05\xCB\x00\x00\x00\x01"; // mock OBEX bytes
        let frame = build_uih(client.target_dlci(), false, None, payload);
        let responses = client.handle_packet(&frame).unwrap();
        assert!(
            responses.is_empty(),
            "no auto-reply on payload — that's the upper layer's job"
        );
        assert_eq!(
            client.take_events(),
            vec![RfcommClientEvent::Payload(payload.to_vec())]
        );
    }

    #[test]
    fn rfcomm_client_build_outbound_uih_wraps_bytes_on_target_dlci() {
        let mut client = open_rfcomm_client();
        finish_msc_exchange(&mut client);
        assert!(client.is_open());

        let frame = client.build_outbound_uih(b"hello").unwrap();
        let parsed = parse_frame(&frame).unwrap();
        assert_eq!(parsed.kind, RfcommFrameKind::Uih);
        assert_eq!(parsed.dlci, client.target_dlci());
        assert!(parsed.command_response, "client's UIH commands have CR=1");
        assert_eq!(parsed.payload, b"hello");
    }

    #[test]
    fn rfcomm_client_build_outbound_uih_errors_before_open() {
        let mut client = RfcommClientState::new(19);
        assert!(client.build_outbound_uih(b"too early").is_err());
        let _ = client.kickoff().unwrap();
        assert!(client.build_outbound_uih(b"still too early").is_err());
    }

    #[test]
    fn rfcomm_client_dm_hard_fails_with_event() {
        let mut client = RfcommClientState::new(19);
        let _ = client.kickoff().unwrap();
        let dm = build_dm(client.target_dlci(), false);
        let _ = client.handle_packet(&dm).unwrap();
        assert!(matches!(client.phase(), RfcommClientPhase::Failed(_)));
        let events = client.take_events();
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], RfcommClientEvent::Failed(reason) if reason.contains("DM")),
            "expected Failed(DM…), got {:?}",
            events
        );
    }

    #[test]
    fn rfcomm_client_disc_emits_closed_event_and_replies_ua() {
        let mut client = open_rfcomm_client();
        finish_msc_exchange(&mut client);
        let _ = client.take_events();

        let disc = build_disc(client.target_dlci(), false);
        let responses = client.handle_packet(&disc).unwrap();
        assert_eq!(responses.len(), 1);
        let parsed = parse_frame(&responses[0]).unwrap();
        assert_eq!(parsed.kind, RfcommFrameKind::Ua);
        assert!(matches!(client.phase(), RfcommClientPhase::Closed));
        assert_eq!(client.take_events(), vec![RfcommClientEvent::Closed]);
    }

    #[test]
    fn rfcomm_client_uih_before_open_is_silently_dropped_not_emitted() {
        let mut client = open_rfcomm_client();
        // We're in AwaitingTargetUa — UIH on target DLCI here would be
        // anomalous; ensure we don't emit a Payload event for it.
        assert!(matches!(
            client.phase(),
            RfcommClientPhase::AwaitingTargetUa
        ));
        let frame = build_uih(client.target_dlci(), false, None, b"early");
        let _ = client.handle_packet(&frame).unwrap();
        assert!(client.take_events().is_empty());
    }

    /// Helper: drive the client through SABM/UA on mux + PN handshake +
    /// SABM on target. Returns a client in `AwaitingTargetUa`'s
    /// successor — specifically the moment after our MSC CMD has gone
    /// out and we're awaiting the peer's MSC bookkeeping. Stops short
    /// of `Open` so each test can drive that final step in the order
    /// it cares about.
    ///
    /// Wait — actually finishes through the UA on target DLCI so the
    /// next step is the MSC exchange. Phase is `AwaitingTargetUa`'s
    /// successor (we sent our MSC CMD; msc_local_pending=true).
    fn open_rfcomm_client() -> RfcommClientState {
        let mut client = RfcommClientState::new(19);
        let _ = client.kickoff().unwrap();
        let _ = client
            .handle_packet(&build_ua(RFCOMM_DLCI_MULTIPLEXER, true))
            .unwrap();
        let pn_rsp = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            false,
            None,
            &build_parameter_negotiation_response(client.target_dlci(), 7, 127, 7),
        );
        let _ = client.handle_packet(&pn_rsp).unwrap();
        let _ = client
            .handle_packet(&build_ua(client.target_dlci(), true))
            .unwrap();
        client
    }

    fn finish_msc_exchange(client: &mut RfcommClientState) {
        let target_dlci = client.target_dlci();
        let msc_rsp = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            false,
            None,
            &build_modem_status_response(target_dlci, RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = client.handle_packet(&msc_rsp).unwrap();
        let peer_msc_cmd = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            false,
            None,
            &build_modem_status_command(target_dlci, RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = client.handle_packet(&peer_msc_cmd).unwrap();
    }

    fn open_hfp_state() -> RfcommState {
        let mut state = RfcommState::new();
        state.set_wbs_supported(true);
        let _ = state
            .handle_packet(&build_sabm(RFCOMM_DLCI_MULTIPLEXER, true))
            .unwrap();
        let _ = state
            .handle_packet(&build_sabm(aokie_hfp_dlci(), true))
            .unwrap();
        // Drive the MSC exchange to completion so the SLC AT queue
        // begins firing. Mirrors the runtime flow: AG acks our MSC CMD
        // with MSC RSP, then sends its own MSC CMD which we ack +
        // simultaneously emit AT+BRSF=949 on.
        let msc_rsp = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_modem_status_response(aokie_hfp_dlci(), RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = state.handle_packet(&msc_rsp).unwrap();
        let msc_cmd = build_uih(
            RFCOMM_DLCI_MULTIPLEXER,
            true,
            None,
            &build_modem_status_command(aokie_hfp_dlci(), RFCOMM_LOCAL_MODEM_STATUS),
        );
        let _ = state.handle_packet(&msc_cmd).unwrap();
        state
    }

    #[test]
    fn take_client_events_for_dlci_keeps_others_queued() {
        // Regression: when MAP and PBAP both ride the inbound HFP
        // shared mux, each runtime must drain *only* its own DLCI's
        // events. The unfiltered drain consumed siblings' events
        // silently, leaving the second runtime stalled at OBEX CONNECT.
        let mut state = RfcommState::default();
        state
            .client_events
            .push(ClientDlciEvent::Opened { dlci: 11 });
        state
            .client_events
            .push(ClientDlciEvent::Opened { dlci: 13 });
        state.client_events.push(ClientDlciEvent::Payload {
            dlci: 11,
            payload: vec![0xa0, 0x00, 0x03],
        });
        state.client_events.push(ClientDlciEvent::Failed {
            dlci: 13,
            reason: "synthetic".to_string(),
        });

        let pbap = state.take_client_events_for_dlci(13);
        assert_eq!(pbap.len(), 2);
        assert!(matches!(pbap[0], ClientDlciEvent::Opened { dlci: 13 }));
        assert!(matches!(pbap[1], ClientDlciEvent::Failed { dlci: 13, .. }));
        // MAP's events must still be queued.
        assert_eq!(state.client_events.len(), 2);

        let map = state.take_client_events_for_dlci(11);
        assert_eq!(map.len(), 2);
        assert!(matches!(map[0], ClientDlciEvent::Opened { dlci: 11 }));
        assert!(matches!(map[1], ClientDlciEvent::Payload { dlci: 11, .. }));
        assert!(state.client_events.is_empty());
    }

    #[test]
    fn fuzz_frame_and_mux_parsers_do_not_panic_on_random_bytes() {
        // RFCOMM rides on top of L2CAP; random L2CAP payloads
        // (bytes that almost-look-like RFCOMM but fail length checks
        // / FCS validation) must surface as Err, not a panic.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xc0ff_eec0_ffee_c0ff);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 512) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_frame(&buf);
            let _ = parse_multiplexer_command(&buf);
        }
    }
}
