pub const HF_FEATURE_EC_NR: u32 = 1 << 0;
/// HFP §4.35.1 HF bit 1: "Call waiting and 3-way calling". Advertised ONLY
/// when the operator turned the holdAndCallWaiting setting on (Phase 4) —
/// advertising it is what makes sending AT+CHLD=? during SLC spec-legal.
pub const HF_FEATURE_CALL_WAITING_3WAY: u32 = 1 << 1;
pub const HF_FEATURE_CLI_PRESENTATION: u32 = 1 << 2;
pub const HF_FEATURE_REMOTE_VOLUME: u32 = 1 << 4;
pub const HF_FEATURE_ENHANCED_CALL_STATUS: u32 = 1 << 5;
pub const HF_FEATURE_CODEC_NEGOTIATION: u32 = 1 << 7;
pub const HF_FEATURE_HF_INDICATORS: u32 = 1 << 8;
pub const HF_FEATURE_ESCO_S4: u32 = 1 << 9;

/// HFP §4.35.1 AG bit 0: "Three-way calling" — the phone-side half of the
/// call-waiting capability gate.
pub const AG_FEATURE_THREE_WAY_CALLING: u32 = 1 << 0;

// HF_FEATURE_HF_INDICATORS is intentionally NOT advertised. Per HFP
// §4.2.1.6, advertising it commits us to the AT+BIND / AT+BIND=? /
// AT+BIND? handshake before any other post-SLC commands. iPhones (and
// most modern AGs) treat the missing BIND exchange as an incomplete
// SLC and drop the link with reason 0x13 right after SLC otherwise
// looks "ready". Until BIND is implemented we just stay at HFP 1.6
// feature semantics, which the AG happily accepts without BIND.
pub const AOKIE_HF_SUPPORTED_FEATURES: u32 = HF_FEATURE_EC_NR
    | HF_FEATURE_CLI_PRESENTATION
    | HF_FEATURE_REMOTE_VOLUME
    | HF_FEATURE_ENHANCED_CALL_STATUS
    | HF_FEATURE_CODEC_NEGOTIATION
    | HF_FEATURE_ESCO_S4;

pub const HFP_CODEC_CVSD: u8 = 1;
pub const HFP_CODEC_MSBC: u8 = 2;

/// One AT+CHLD call-hold action (HFP §4.34.2 / 3GPP 27.007 §7.13). Note the
/// spec semantics the Phase-4 review pinned down: `2` is the ONLY
/// non-destructive hold/accept/swap ("place active on hold, accept
/// waiting/held"); `1` RELEASES the active call first. Indexed forms need the
/// AG to have advertised the `1x`/`2x` enhanced modes in its +CHLD list.
/// Builders only for now — nothing sends these until the switchboard slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChldAction {
    /// AT+CHLD=0 — release all held calls (or set UDUB on a waiting call).
    ReleaseAllHeld,
    /// AT+CHLD=1 — DESTRUCTIVE: release the active call, accept the other.
    ReleaseActiveAcceptOther,
    /// AT+CHLD=1<idx> — release exactly one call by CLCC index.
    ReleaseSpecific(u8),
    /// AT+CHLD=2 — hold the active call, accept the waiting/held one. Also
    /// the plain swap/resume. NEVER blind-retry it: a repeat after a lost OK
    /// swaps straight back.
    HoldActiveAcceptOther,
    /// AT+CHLD=2<idx> — private consultation with one call by CLCC index.
    PrivateConsult(u8),
    /// AT+CHLD=3 — add the held call to the conversation (multiparty).
    Merge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfpAtCommand {
    SupportedFeatures {
        /// Advertise HF_FEATURE_CALL_WAITING_3WAY (Phase 4 setting).
        call_waiting: bool,
    },
    /// `AT+BAC=...` — codec list the AG can pick from.
    /// `wbs_supported = true`  → "AT+BAC=1,2" (CVSD + mSBC)
    /// `wbs_supported = false` → "AT+BAC=1"  (CVSD only)
    /// Set false when the dongle doesn't expose alt 6 (the standards-
    /// compliant mSBC iso pipe shape) — routing mSBC through alt 2 on
    /// such dongles produces unusable static; better to stay on CVSD.
    AvailableCodecs {
        wbs_supported: bool,
    },
    RetrieveIndicators,
    RetrieveIndicatorStatus,
    RetrieveCallHoldSupport,
    /// `AT+CCWA=1` — ask the AG to forward network call-waiting
    /// notifications (+CCWA) while a call is active (Phase 4). Enabling the
    /// NOTIFICATION only — it neither provisions nor verifies the carrier's
    /// call-waiting service (3GPP 27.007 §7.12), which is why the capability
    /// verdict reads "network unverified" until a real waiting call proves it.
    EnableCallWaitingNotifications,
    /// `AT+CHLD=<action>` — call-hold / multiparty control (Phase 4).
    CallHold(ChldAction),
    ActivateClip(bool),
    EnableIndicatorUpdates(bool),
    EnableAllIndicatorStatusUpdates(bool),
    /// `AT+NREC=0` — tell the AG we're handling echo cancel / noise
    /// reduction ourselves and it should ship raw mic audio over SCO.
    /// We advertise `HF_FEATURE_EC_NR` in BRSF, so the AG accepts the
    /// command. Some Pixel firmware (and Bluedroid in general) won't
    /// route mic audio over the SCO IN endpoint until the HF has
    /// either explicitly disabled NREC or sent another post-SLC
    /// configure — without this command the iso IN pipe stays at
    /// zero-length packets for the entire call even though the link
    /// is otherwise healthy (transparent air mode, eSCO T2 timing,
    /// alt setting 2 active).
    DisableNoiseReduction,
    Answer,
    RejectOrHangup,
    /// `ATD<number>;` — place an OUTBOUND voice call (Phase 2, call-policy
    /// spec). The trailing `;` marks a voice (not data) call per 3GPP 27.007.
    /// The number must be pre-sanitized (digits and a leading `+` only) —
    /// the builder strips anything else defensively so a malformed number
    /// can never smuggle extra AT syntax onto the wire.
    Dial(String),
    ConfirmCodec(u8),
    /// `AT+CLCC` — list current calls. Sent right after a call is answered:
    /// instant auto-answer races the ringing-phase `+CLIP` (several live
    /// calls ended with no caller id at all, 2026-07-13), and the +CLCC
    /// response carries the active call's number deterministically.
    ListCurrentCalls,
}

/// One `+CLCC:` line — `<idx>,<dir>,<stat>,<mode>,<mpty>[,"<number>",<type>]`.
/// `status`: 0 active, 1 held, 2 dialing (MO), 3 alerting (MO), 4 incoming
/// (MT), 5 waiting (MT). CLCC snapshots are the authoritative multi-call
/// topology source (Phase 4) — indicators only carry edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClccEntry {
    pub index: u8,
    /// 0 = mobile-originated (we/owner dialed), 1 = mobile-terminated.
    pub direction: u8,
    pub status: u8,
    /// 0 = voice; anything else is data/fax and never ours.
    pub mode: u8,
    pub multiparty: bool,
    pub number: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfpAgResult {
    Ok,
    Error,
    Ring,
    SupportedFeatures(u32),
    Indicators(Vec<String>),
    IndicatorStatus(Vec<i32>),
    IndicatorUpdate {
        index: u8,
        value: i32,
    },
    CallerId(String),
    /// `+CHLD: (…)` — the AG's supported call-hold modes (SLC probe reply).
    CallHoldModes(Vec<String>),
    /// `+CCWA: "<number>",…` — unsolicited network call-waiting notification.
    CallWaitingNotification(String),
    /// One parsed `+CLCC:` line.
    CallListEntry(ClccEntry),
    SelectedCodec(u8),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfpEvent {
    ServiceLevelConnectionReady,
    /// AG returned ERROR for one of the SLC commands. Carries a short
    /// description of which command failed so callers/log lines can tell
    /// `BRSF` from `CIND` failures. The SLC must be torn down — there's
    /// no "retry the queue" path that's safe to take.
    ServiceLevelConnectionFailed(String),
    IncomingCall,
    /// `callsetup,3` — an OUTBOUND (mobile-originated) call is alerting at
    /// the remote end. MT calls never produce callsetup 3, so this has
    /// always been the MO-alerting signal.
    Ringing,
    /// `callsetup,2` — an OUTBOUND call setup started (Phase 2): either the
    /// plugin dialed (ATD) or the phone's OWNER dialed from the handset.
    /// Consumers must treat the pending call as outbound — the eventual
    /// `CallAnswered` is the REMOTE party picking up, never a caller to
    /// greet.
    OutgoingDialing,
    CallAnswered,
    CallTerminated,
    CallerId(String),
    /// Phase 4: a SECOND caller is knocking while a call is active. Emitted
    /// once per waiting episode; `None` when the number isn't known yet (the
    /// +CCWA usually beats the callsetup edge on Android, so it's normally
    /// `Some`). A later +CCWA that names a so-far-anonymous waiting caller
    /// re-emits with `Some(number)`.
    CallWaiting(Option<String>),
    /// The waiting episode ended without the active call changing — the
    /// waiting caller hung up, or the owner dealt with it on the handset.
    CallWaitingEnded,
    /// `callheld` CIEV transition: 0 = none held, 1 = held + active swap
    /// state, 2 = held with no active call.
    CallHeld(i32),
    /// One `+CLCC:` line (observe-only topology; consumers assemble the
    /// snapshot between the CLCC request and its OK).
    CallListEntry(ClccEntry),
    CodecSelected {
        codec: String,
        sample_rate: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfpHandsFreeState {
    service_level_ready: bool,
    supported_features_exchanged: bool,
    clip_enabled: bool,
    indicator_updates_enabled: bool,
    call_indicator_index: u8,
    callsetup_indicator_index: u8,
    call_active: bool,
    incoming_call: bool,
    /// An OUTBOUND call setup is in progress (`callsetup` 2 or 3 with no
    /// active call). Mirrors `incoming_call` for the MO direction (Phase 2):
    /// a `callsetup,0` while set is the same ambiguous edge — abandon vs
    /// answered-with-late-`call,1` — so it feeds the SAME held verdict.
    outgoing_setup: bool,
    /// The +CIND=? DEFINITIONS response was parsed and named BOTH the call
    /// and callsetup indicators. Without it the index fields above are the
    /// built-in defaults, which do NOT match every phone (Android puts
    /// `call` FIRST) — a ringing callsetup CIEV then misreads as
    /// CallAnswered (live phantom-answer incidents 2026-07-14/15). The SLC
    /// pump re-requests the definitions ONCE when readiness is reached
    /// without them.
    indicator_definitions_seen: bool,
    indicator_definitions_retry_used: bool,
    // ── Phase 4: call waiting / hold capability + episode state ─────────
    /// HF-side half of the gate: the holdAndCallWaiting setting (env
    /// AOKIE_CALL_WAITING at radio start). false = byte-for-byte legacy
    /// behaviour: BRSF bit 1 off, no AT+CHLD=? probe, no AT+CCWA=1, and the
    /// callsetup-while-active heuristic keeps its lost-call,0 recovery.
    call_waiting_enabled: bool,
    /// The AG's +BRSF feature word (None until the BRSF response lands).
    ag_features: Option<u32>,
    /// Parsed `+CHLD: (…)` modes, e.g. ["0","1","1x","2","2x","3"].
    chld_modes: Option<Vec<String>>,
    /// AT+CCWA=1 was answered OK (notifications armed).
    ccwa_accepted: bool,
    /// The AT+CHLD=?/AT+CCWA=1 probe was refused (ERROR) — capability off,
    /// SLC unharmed. Remembered so the readiness verdict names it.
    probe_refused: Option<&'static str>,
    /// `callheld` position from the CIND definitions, when the AG lists it.
    callheld_indicator_index: Option<u8>,
    /// Last `callheld` indicator value (0/1/2).
    call_held: i32,
    /// A waiting caller is currently knocking (episode active).
    call_waiting: bool,
    /// The current waiting episode already carried a number in its event.
    waiting_number_known: bool,
    /// `callsetup: 0` arrived while ringing with the `call` indicator still 0.
    /// That transition is AMBIGUOUS — the ring phase ends for BOTH an
    /// abandoned ring AND an answered call, and some AGs send `callsetup,0`
    /// BEFORE `call,1` (observed live 2026-07-13: the old immediate
    /// CallTerminated killed the plugin's call session ~4s into a REAL
    /// answered call — the phone call stayed up but every subsequent audio
    /// frame was dropped and the receptionist went deaf, AOK-CTRL-001). The
    /// verdict is HELD here until the `call` indicator speaks: a `call,1`
    /// CIEV (or a CIND? snapshot showing call=1) resolves it as the answer;
    /// a `call,0` CIEV, a call=0 snapshot (the ACL-keepalive poll arrives
    /// within ~8s of a quiet line) or a fresh callsetup resolves it as the
    /// abandon and emits the terminate then.
    terminate_pending: bool,
    selected_codec: Option<u8>,
}

impl Default for HfpHandsFreeState {
    fn default() -> Self {
        Self {
            service_level_ready: false,
            supported_features_exchanged: false,
            clip_enabled: false,
            indicator_updates_enabled: false,
            call_indicator_index: 2,
            callsetup_indicator_index: 3,
            indicator_definitions_seen: false,
            indicator_definitions_retry_used: false,
            call_waiting_enabled: false,
            ag_features: None,
            chld_modes: None,
            ccwa_accepted: false,
            probe_refused: None,
            callheld_indicator_index: None,
            call_held: 0,
            call_waiting: false,
            waiting_number_known: false,
            call_active: false,
            incoming_call: false,
            outgoing_setup: false,
            terminate_pending: false,
            selected_codec: None,
        }
    }
}

impl HfpHandsFreeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn initial_service_level_commands(
        wbs_supported: bool,
        call_waiting: bool,
    ) -> Vec<HfpAtCommand> {
        // RetrieveCallHoldSupport and EnableCallWaitingNotifications are
        // queued unconditionally but SKIPPED at pop time by
        // `should_skip_slc_command` unless call_waiting is on AND the AG's
        // +BRSF (which lands before either would be sent) advertises
        // three-way calling — sending AT+CHLD=? to an AG that never claimed
        // the feature is out of spec and some AGs answer ERROR.
        vec![
            HfpAtCommand::SupportedFeatures { call_waiting },
            HfpAtCommand::AvailableCodecs { wbs_supported },
            HfpAtCommand::RetrieveIndicators,
            HfpAtCommand::RetrieveIndicatorStatus,
            HfpAtCommand::RetrieveCallHoldSupport,
            HfpAtCommand::EnableCallWaitingNotifications,
            HfpAtCommand::ActivateClip(true),
            HfpAtCommand::EnableIndicatorUpdates(true),
            HfpAtCommand::EnableAllIndicatorStatusUpdates(true),
            HfpAtCommand::DisableNoiseReduction,
        ]
    }

    /// Phase 4 gate, HF side. Set before the SLC kicks off (both pumps set
    /// it from the same flag that picks the BRSF word).
    pub fn set_call_waiting_enabled(&mut self, enabled: bool) {
        self.call_waiting_enabled = enabled;
    }

    pub fn call_waiting_enabled(&self) -> bool {
        self.call_waiting_enabled
    }

    /// The AG advertised three-way calling in +BRSF. False until the BRSF
    /// response has been parsed.
    pub fn ag_supports_three_way(&self) -> bool {
        self.ag_features
            .is_some_and(|f| f & AG_FEATURE_THREE_WAY_CALLING != 0)
    }

    /// True when this SLC command should be silently dropped instead of
    /// sent: the call-waiting probes are only spec-legal (and only useful)
    /// when both sides advertise three-way calling.
    pub fn should_skip_slc_command(&self, command: &HfpAtCommand) -> bool {
        match command {
            HfpAtCommand::RetrieveCallHoldSupport
            | HfpAtCommand::EnableCallWaitingNotifications => {
                !(self.call_waiting_enabled && self.ag_supports_three_way())
            }
            _ => false,
        }
    }

    /// The two capability probes are NON-FATAL on ERROR (the Phase-4 review's
    /// first bug): an AG refusing AT+CHLD=? / AT+CCWA=1 merely lacks call
    /// waiting — it must not cost the whole HFP connection. Returns true when
    /// the failed command was one of them, in which case the pump continues
    /// the SLC queue exactly as if the command had succeeded.
    pub fn note_slc_probe_error(&mut self, command: &HfpAtCommand) -> bool {
        match command {
            HfpAtCommand::RetrieveCallHoldSupport => {
                self.chld_modes = None;
                self.probe_refused = Some("AT+CHLD=?");
                eprintln!(
                    "[AokieRadio] AG refused AT+CHLD=? — call waiting/hold unsupported on this phone (SLC continues)"
                );
                true
            }
            HfpAtCommand::EnableCallWaitingNotifications => {
                self.ccwa_accepted = false;
                self.probe_refused = Some("AT+CCWA=1");
                eprintln!(
                    "[AokieRadio] AG refused AT+CCWA=1 — call-waiting notifications unavailable (SLC continues)"
                );
                true
            }
            _ => false,
        }
    }

    /// Called by the pump when AT+CCWA=1 was answered OK.
    pub fn note_ccwa_accepted(&mut self) {
        self.ccwa_accepted = true;
    }

    /// The AG's +CHLD mode list contains this mode (e.g. "1", "2", "2x").
    pub fn chld_mode_supported(&self, mode: &str) -> bool {
        self.chld_modes
            .as_ref()
            .is_some_and(|m| m.iter().any(|v| v == mode))
    }

    /// The full Phase-4 capability gate: setting on, AG three-way bit, a
    /// usable +CHLD mode list (at least plain 1 and 2), and +CCWA armed.
    /// Carrier provisioning can NOT be proven at SLC time — a true verdict
    /// here still reads "ready (network unverified)" until a real waiting
    /// call arrives.
    pub fn three_way_negotiated(&self) -> bool {
        self.call_waiting_enabled
            && self.ag_supports_three_way()
            && self.chld_mode_supported("1")
            && self.chld_mode_supported("2")
            && self.ccwa_accepted
    }

    /// One log line naming the call-waiting capability verdict for this
    /// connection — the diagnosis anchor for every Phase-4 incident, same
    /// pattern as the CIND-definitions line.
    pub fn log_call_waiting_verdict(&self) {
        if !self.call_waiting_enabled {
            eprintln!("[AokieRadio] call waiting: disabled (holdAndCallWaiting off)");
            return;
        }
        if self.three_way_negotiated() {
            eprintln!(
                "[AokieRadio] call waiting: ready, network unverified (AG 3-way ok, +CHLD modes [{}], +CCWA armed, callheld indicator {})",
                self.chld_modes.as_deref().unwrap_or(&[]).join(","),
                match self.callheld_indicator_index {
                    Some(i) => format!("index {i}"),
                    None => "MISSING".to_string(),
                },
            );
        } else {
            eprintln!(
                "[AokieRadio] call waiting: unsupported on this connection (AG 3-way {}, +CHLD {}, +CCWA {}{})",
                if self.ag_supports_three_way() { "ok" } else { "missing" },
                match &self.chld_modes {
                    Some(m) => format!("[{}]", m.join(",")),
                    None => "not answered".to_string(),
                },
                if self.ccwa_accepted { "armed" } else { "off" },
                match self.probe_refused {
                    Some(p) => format!("; {p} refused"),
                    None => String::new(),
                },
            );
        }
    }

    pub fn apply_result(&mut self, result: &HfpAgResult) -> Vec<HfpEvent> {
        match result {
            HfpAgResult::Ok => {
                self.supported_features_exchanged = true;
                Vec::new()
            }
            HfpAgResult::Ring => {
                // A live RING while a terminate verdict was held means the
                // ring never actually ended — drop the held verdict.
                self.terminate_pending = false;
                if self.call_waiting && self.call_held != 0 {
                    // The knocker's ring re-presenting while another call is
                    // HELD — the same queue-jump hazard as the suppressed
                    // promotion in update_call_state: the episode is already
                    // tracked and the FIFO cascade owns accepting it. Never
                    // mint an IncomingCall that auto-answer would grab ahead
                    // of the held caller.
                    return Vec::new();
                }
                self.incoming_call = true;
                vec![HfpEvent::IncomingCall, HfpEvent::Ringing]
            }
            HfpAgResult::Indicators(indicators) => {
                self.apply_indicator_definitions(indicators);
                Vec::new()
            }
            HfpAgResult::IndicatorUpdate { index, value } => self.apply_indicator(*index, *value),
            HfpAgResult::IndicatorStatus(values) => self.apply_indicator_status(values),
            HfpAgResult::CallerId(number) => vec![HfpEvent::CallerId(number.clone())],
            HfpAgResult::SupportedFeatures(features) => {
                self.ag_features = Some(*features);
                Vec::new()
            }
            HfpAgResult::CallHoldModes(modes) => {
                self.chld_modes = Some(modes.clone());
                Vec::new()
            }
            HfpAgResult::CallWaitingNotification(number) => {
                // +CCWA fires only while a call is active (the AG forwards
                // the network notification); anything else is noise. Repeats
                // arrive like RING (~every 5s on Android) — one event per
                // episode, plus one upgrade if the episode started anonymous.
                if !self.call_active {
                    Vec::new()
                } else if !self.call_waiting {
                    self.call_waiting = true;
                    self.waiting_number_known = true;
                    vec![HfpEvent::CallWaiting(Some(number.clone()))]
                } else if !self.waiting_number_known {
                    self.waiting_number_known = true;
                    vec![HfpEvent::CallWaiting(Some(number.clone()))]
                } else {
                    Vec::new()
                }
            }
            HfpAgResult::CallListEntry(entry) => {
                let mut events = Vec::new();
                // The +CLCC caller-id rescue (2026-07-13) — but only for the
                // ACTIVE or plain-incoming leg. The old first-quoted-number
                // parse fed EVERY line into the CallerId pipeline, so with a
                // held or waiting second call the LAST line won and the
                // NEIGHBOUR's number could overwrite the active caller's.
                if matches!(entry.status, 0 | 4) {
                    if let Some(number) = &entry.number {
                        events.push(HfpEvent::CallerId(number.clone()));
                    }
                }
                events.push(HfpEvent::CallListEntry(entry.clone()));
                events
            }
            HfpAgResult::SelectedCodec(codec) => {
                self.selected_codec = Some(*codec);
                vec![codec_event(*codec)]
            }
            HfpAgResult::Error | HfpAgResult::Unknown(_) => Vec::new(),
        }
    }

    pub fn mark_command_sent(&mut self, command: &HfpAtCommand) {
        match command {
            HfpAtCommand::ActivateClip(active) => self.clip_enabled = *active,
            HfpAtCommand::EnableIndicatorUpdates(active) => {
                self.indicator_updates_enabled = *active;
            }
            HfpAtCommand::Dial(_) => {
                // Phase 2 (live incident 2026-07-14, silent callback call):
                // dialing means the consumer considers any previous call
                // OVER — it only dials on an idle line. A verdict still HELD
                // for a just-abandoned inbound ring (its session was closed
                // via the parallel SCO-teardown path, bypassing this state
                // machine) must never discharge against the NEW attempt: the
                // stale CallTerminated fired 100ms after ATD, killed the
                // fresh dial session, and the callee answered a silent line.
                // Align this machine with the dialer's reality and pre-arm
                // the outbound setup.
                self.terminate_pending = false;
                self.incoming_call = false;
                self.call_active = false;
                self.outgoing_setup = true;
            }
            _ => {}
        }
    }

    pub fn call_active(&self) -> bool {
        self.call_active
    }

    pub fn incoming_call(&self) -> bool {
        self.incoming_call
    }

    pub fn service_level_ready(&self) -> bool {
        self.service_level_ready
    }

    pub fn call_indicator_index(&self) -> u8 {
        self.call_indicator_index
    }

    pub fn callsetup_indicator_index(&self) -> u8 {
        self.callsetup_indicator_index
    }

    pub fn mark_service_level_ready(&mut self) -> Option<HfpEvent> {
        if self.service_level_ready {
            return None;
        }
        self.service_level_ready = true;
        // Phase 4 diagnosis anchor: name the call-waiting verdict once per
        // SLC, right where the CIND-definitions line lives.
        self.log_call_waiting_verdict();
        Some(HfpEvent::ServiceLevelConnectionReady)
    }

    fn apply_indicator_definitions(&mut self, indicators: &[String]) {
        let mut saw_call = false;
        let mut saw_callsetup = false;
        for (index, name) in indicators.iter().enumerate() {
            let hfp_index = (index + 1) as u8;
            if name.eq_ignore_ascii_case("call") {
                self.call_indicator_index = hfp_index;
                saw_call = true;
            } else if name.eq_ignore_ascii_case("callsetup") {
                self.callsetup_indicator_index = hfp_index;
                saw_callsetup = true;
            } else if name.eq_ignore_ascii_case("callheld") {
                self.callheld_indicator_index = Some(hfp_index);
            }
        }
        if saw_call && saw_callsetup {
            self.indicator_definitions_seen = true;
        }
        // The one log line that would have named every phantom-answer
        // incident instantly: the mapping THIS connection will use.
        eprintln!(
            "[AokieRadio] CIND definitions parsed: call={} callsetup={} callheld={} ({} indicators{})",
            self.call_indicator_index,
            self.callsetup_indicator_index,
            self.callheld_indicator_index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".to_string()),
            indicators.len(),
            if saw_call && saw_callsetup {
                ""
            } else {
                " — INCOMPLETE, call/callsetup not both named"
            },
        );
    }

    /// True exactly once: SLC readiness was reached WITHOUT a usable
    /// definitions line (lost/fragmented/corrupted on the wire) — the pump
    /// should re-queue AT+CIND=? + AT+CIND? instead of going ready on
    /// default indices that may not match this phone.
    pub fn needs_indicator_definitions_retry(&mut self) -> bool {
        if self.indicator_definitions_seen || self.indicator_definitions_retry_used {
            return false;
        }
        self.indicator_definitions_retry_used = true;
        eprintln!(
            "[AokieRadio] SLC finished without indicator DEFINITIONS — re-requesting AT+CIND=? once (default call/callsetup indices may not match this phone)"
        );
        true
    }

    /// Sync internal call-state from a `+CIND?` status read.
    ///
    /// Per HFP §4.2.1.2 the AT+CIND? response is a *snapshot* the HF
    /// uses to align its initial state with the AG — not a notification.
    /// The notification stream is +CIEV (gated by AT+CMER), which fires
    /// from `apply_indicator`. Treating the snapshot as a notification
    /// caused phantom IncomingCall events when an AG (Pixel observed)
    /// reported callsetup=1 in its initial CIND? — likely a leftover
    /// from a recent missed-call/notification — making the HF auto-answer
    /// into a void: no real call to answer → no codec negotiation → no
    /// SCO → dashboard stuck on "Unknown Caller". We now sync the
    /// fields silently and rely on the next +CIEV transition (or RING)
    /// to emit user-visible call events.
    fn apply_indicator_status(&mut self, values: &[i32]) -> Vec<HfpEvent> {
        let mut events = Vec::new();
        if let Some(call) = indicator_status_value(values, self.call_indicator_index) {
            let active = *call != 0;
            // The snapshot stays silent EXCEPT to resolve a held ring
            // verdict (see `terminate_pending`): the `+CIEV: call` edge that
            // would decide answer-vs-abandon may have been lost, and the
            // consumer is owed exactly one resolution. The ACL-keepalive
            // AT+CIND? poll makes this land within ~8s of a quiet line.
            if self.terminate_pending {
                self.terminate_pending = false;
                if active && !self.call_active {
                    events.push(HfpEvent::CallAnswered);
                } else if !active {
                    events.push(HfpEvent::CallTerminated);
                }
            }
            self.call_active = active;
            if active {
                // An already-active call at SLC time — make sure
                // incoming_call doesn't shadow it.
                self.incoming_call = false;
            }
        }
        if let Some(callsetup) = indicator_status_value(values, self.callsetup_indicator_index) {
            // Only callsetup=1 (incoming) needs to be reflected so we
            // don't auto-answer phantom rings. Outgoing setup (2/3) is
            // synced SILENTLY too (Phase 2): a snapshot is state alignment,
            // not a notification — but the held-verdict logic needs to know
            // an MO attempt is pending so its callsetup,0 resolves honestly.
            self.incoming_call = *callsetup == 1 && !self.call_active;
            self.outgoing_setup = (*callsetup == 2 || *callsetup == 3) && !self.call_active;
            if *callsetup == 0 && self.call_waiting {
                // A stuck waiting episode whose ending edge was lost: align
                // silently (snapshots are not notifications) — the plugin's
                // episode bookkeeping is keyed by call id and self-heals.
                eprintln!(
                    "[AokieRadio] CIND snapshot shows callsetup=0 while a waiting episode was flagged — clearing it"
                );
                self.call_waiting = false;
                self.waiting_number_known = false;
            }
        }
        if let Some(index) = self.callheld_indicator_index {
            if let Some(held) = indicator_status_value(values, index) {
                // Silent state alignment, same rule as the other indicators.
                self.call_held = *held;
            }
        }
        events
    }

    fn apply_indicator(&mut self, index: u8, value: i32) -> Vec<HfpEvent> {
        if index == self.call_indicator_index {
            self.update_call_state(value != 0)
        } else if index == self.callsetup_indicator_index {
            self.update_callsetup(value)
        } else if Some(index) == self.callheld_indicator_index {
            self.update_callheld(value)
        } else {
            Vec::new()
        }
    }

    fn update_callheld(&mut self, value: i32) -> Vec<HfpEvent> {
        if value == self.call_held {
            return Vec::new();
        }
        self.call_held = value;
        vec![HfpEvent::CallHeld(value)]
    }

    fn update_callsetup(&mut self, value: i32) -> Vec<HfpEvent> {
        match value {
            0 => {
                if self.call_waiting {
                    // The waiting caller stopped waiting: they hung up, or
                    // the owner handled them on the handset (a CHLD action
                    // there also shows up as callheld transitions). The
                    // ACTIVE call is untouched — this edge must never feed
                    // the held terminate verdict.
                    self.call_waiting = false;
                    self.waiting_number_known = false;
                    return vec![HfpEvent::CallWaitingEnded];
                }
                if (self.incoming_call || self.outgoing_setup) && !self.call_active {
                    // AMBIGUOUS: the setup phase ends on BOTH abandon and
                    // answer, and `call,1` may arrive AFTER this (observed
                    // live — see `terminate_pending`). The SAME edge exists
                    // for an outbound attempt: remote answered (call,1 next)
                    // vs no-answer/busy/cancel. Never guess "terminated"
                    // here: hold the verdict for the `call` indicator (CIEV
                    // edge or CIND? snapshot) to decide.
                    self.incoming_call = false;
                    self.outgoing_setup = false;
                    self.terminate_pending = true;
                }
                Vec::new()
            }
            1 => {
                // A fresh incoming-call setup while the AG `call` indicator
                // is still active means the previous call's `+CIEV: call,0`
                // was lost or coalesced — e.g. back-to-back / call-waiting
                // calls where `call` never returned to 0, or an SCO drop
                // whose CallTerminated never drove this state machine. Since
                // CallAnswered is edge-triggered on call 0->1 (see
                // update_call_state), a stuck-true call_active would swallow
                // the next answer's CallAnswered. The consumer clears its
                // per-call state (conversation history, greeting, call id)
                // ONLY on CallAnswered, so the previous call's conversation
                // would leak into this one. Synthesize the missed terminate
                // here so the upcoming answer is a genuine 0->1 edge and the
                // full per-call reset runs. Aokie serves one call at a time,
                // so a fresh incoming setup is an unambiguous prior-call
                // boundary; emitting CallTerminated first also lets the
                // stranded prior call finish its post-call processing.
                //
                // Phase 4 carve-out: when call waiting is NEGOTIATED on this
                // connection, callsetup=1 during an active call is a REAL
                // second caller knocking — the lost-call,0 recovery below
                // would kill the live conversation mid-sentence and try to
                // ATA the waiting call (which plain ATA cannot answer). The
                // episode usually starts at the +CCWA (which carries the
                // number and beats this edge on Android); this arm covers a
                // callsetup-first ordering with an anonymous start.
                if self.call_active && self.three_way_negotiated() {
                    if !self.call_waiting {
                        self.call_waiting = true;
                        self.waiting_number_known = false;
                        return vec![HfpEvent::CallWaiting(None)];
                    }
                    return Vec::new();
                }
                let mut events = Vec::new();
                if self.terminate_pending {
                    // A NEW ring while the previous ring's verdict was still
                    // held: nothing ever answered it — that ring is over.
                    self.terminate_pending = false;
                    events.push(HfpEvent::CallTerminated);
                } else if self.call_active {
                    self.call_active = false;
                    events.push(HfpEvent::CallTerminated);
                }
                self.incoming_call = true;
                self.outgoing_setup = false;
                events.push(HfpEvent::IncomingCall);
                events
            }
            2 => {
                // OUTBOUND setup started (we sent ATD, or the owner dialed on
                // the handset). Mirror the fresh-incoming logic: a still-held
                // verdict or a stuck-active call means the PREVIOUS call's
                // boundary was lost — resolve it first so the consumer's
                // per-call reset runs before the new outbound session.
                let mut events = Vec::new();
                if self.terminate_pending {
                    self.terminate_pending = false;
                    events.push(HfpEvent::CallTerminated);
                } else if self.call_active {
                    self.call_active = false;
                    events.push(HfpEvent::CallTerminated);
                }
                self.incoming_call = false;
                self.outgoing_setup = true;
                events.push(HfpEvent::OutgoingDialing);
                events
            }
            3 => {
                // MO alerting. Some AGs jump straight to 3 without a 2 —
                // make sure the outbound setup is tracked either way.
                if !self.call_active {
                    self.outgoing_setup = true;
                }
                vec![HfpEvent::Ringing]
            }
            _ => Vec::new(),
        }
    }

    fn update_call_state(&mut self, active: bool) -> Vec<HfpEvent> {
        if active {
            // Resolves a held callsetup-drop: it was the ANSWER transition
            // (the AG just sent `callsetup,0` before `call,1`).
            self.terminate_pending = false;
        }
        if active == self.call_active {
            if !active && self.terminate_pending {
                // `call: 0` while a verdict is held is the AG's explicit
                // word that nothing is active — the ring was ABANDONED.
                self.terminate_pending = false;
                return vec![HfpEvent::CallTerminated];
            }
            return Vec::new();
        }

        self.call_active = active;
        self.incoming_call = false;
        self.outgoing_setup = false;
        if active {
            vec![HfpEvent::CallAnswered]
        } else if self.call_waiting {
            if self.call_held != 0 {
                // A call is still HELD: the switchboard owns this topology.
                // Promoting here would auto-answer the knocker AHEAD of the
                // held caller (live 2026-07-15: the third caller queue-jumped
                // past the parked one and heard the cold-open greeting while
                // they kept waiting). Keep the waiting episode alive — the
                // plugin's FIFO cascade accepts the knocker with their queue
                // position (AT+CHLD=2 prefers the waiting leg), then swaps
                // back to the held caller.
                return vec![HfpEvent::CallTerminated];
            }
            // The active call ended while a second caller was still
            // knocking and NOTHING is held: the phone keeps ringing them,
            // but callsetup is ALREADY 1 so no fresh edge will announce it.
            // Promote the waiting episode to a normal incoming ring —
            // auto-answer and the whole per-call reset treat it like any
            // fresh call.
            self.call_waiting = false;
            self.waiting_number_known = false;
            self.incoming_call = true;
            vec![
                HfpEvent::CallWaitingEnded,
                HfpEvent::CallTerminated,
                HfpEvent::IncomingCall,
            ]
        } else {
            vec![HfpEvent::CallTerminated]
        }
    }
}

fn indicator_status_value(values: &[i32], indicator_index: u8) -> Option<&i32> {
    values.get(usize::from(indicator_index.checked_sub(1)?))
}

pub fn build_at_command(command: HfpAtCommand) -> Vec<u8> {
    let line = match command {
        HfpAtCommand::SupportedFeatures { call_waiting } => {
            let features = if call_waiting {
                AOKIE_HF_SUPPORTED_FEATURES | HF_FEATURE_CALL_WAITING_3WAY
            } else {
                AOKIE_HF_SUPPORTED_FEATURES
            };
            format!("AT+BRSF={features}\r")
        }
        // CVSD (8 kHz) is always offered. mSBC (16 kHz) only when the
        // dongle exposes the standards-compliant alt 6 SCO interface;
        // running mSBC through alt 2 on dongles that don't (e.g. CSR8510)
        // produces unusable static, so we stay on CVSD there. The AG
        // picks via +BCS:<n>.
        HfpAtCommand::AvailableCodecs { wbs_supported } => {
            if wbs_supported {
                format!("AT+BAC={},{}\r", HFP_CODEC_CVSD, HFP_CODEC_MSBC)
            } else {
                format!("AT+BAC={}\r", HFP_CODEC_CVSD)
            }
        }
        HfpAtCommand::RetrieveIndicators => "AT+CIND=?\r".to_string(),
        HfpAtCommand::RetrieveIndicatorStatus => "AT+CIND?\r".to_string(),
        HfpAtCommand::RetrieveCallHoldSupport => "AT+CHLD=?\r".to_string(),
        HfpAtCommand::EnableCallWaitingNotifications => "AT+CCWA=1\r".to_string(),
        HfpAtCommand::CallHold(action) => match action {
            ChldAction::ReleaseAllHeld => "AT+CHLD=0\r".to_string(),
            ChldAction::ReleaseActiveAcceptOther => "AT+CHLD=1\r".to_string(),
            ChldAction::ReleaseSpecific(index) => format!("AT+CHLD=1{index}\r"),
            ChldAction::HoldActiveAcceptOther => "AT+CHLD=2\r".to_string(),
            ChldAction::PrivateConsult(index) => format!("AT+CHLD=2{index}\r"),
            ChldAction::Merge => "AT+CHLD=3\r".to_string(),
        },
        HfpAtCommand::ActivateClip(active) => format!("AT+CLIP={}\r", u8::from(active)),
        HfpAtCommand::EnableIndicatorUpdates(active) => {
            format!("AT+CMER=3,0,0,{}\r", u8::from(active))
        }
        HfpAtCommand::EnableAllIndicatorStatusUpdates(active) => {
            if active {
                "AT+BIA=1,1,1,1,1,1,1\r".to_string()
            } else {
                "AT+BIA=0,0,0,0,0,0,0\r".to_string()
            }
        }
        HfpAtCommand::DisableNoiseReduction => "AT+NREC=0\r".to_string(),
        HfpAtCommand::Answer => "ATA\r".to_string(),
        HfpAtCommand::RejectOrHangup => "AT+CHUP\r".to_string(),
        HfpAtCommand::Dial(number) => {
            let sanitized: String = number
                .chars()
                .enumerate()
                .filter(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '+'))
                .map(|(_, c)| c)
                .collect();
            format!("ATD{sanitized};\r")
        }
        HfpAtCommand::ConfirmCodec(codec) => format!("AT+BCS={}\r", codec),
        HfpAtCommand::ListCurrentCalls => "AT+CLCC\r".to_string(),
    };
    line.into_bytes()
}

pub fn parse_ag_results(payload: &[u8]) -> Result<Vec<HfpAgResult>, String> {
    let mut carry = String::new();
    parse_ag_results_buffered(&mut carry, payload)
}

/// Per-frame AT parsing with an incomplete trailing line CARRIED to the next
/// frame. AT responses are \r\n framed, but one response line can span two
/// RFCOMM UIH frames — observed live 2026-07-14: the +CIND=? indicator
/// DEFINITIONS line fragmented on an initiator-mux SLC, the mapping fell
/// back to the built-in call/callsetup indices, and on a phone whose real
/// order differs every ringing CIEV then misread as CallAnswered — the
/// phantom "active" session made auto-answer skip every real ring until a
/// manual reconnect. Everything after the last \r/\n is held (bounded 1 KiB
/// — a pathological unterminated stream loses that line, same as before)
/// and prepended to the next frame, so a split line reassembles instead of
/// parsing as two garbage halves.
pub fn parse_ag_results_buffered(
    carry: &mut String,
    payload: &[u8],
) -> Result<Vec<HfpAgResult>, String> {
    let text =
        std::str::from_utf8(payload).map_err(|err| format!("HFP payload is not UTF-8: {err}"))?;
    let mut whole = std::mem::take(carry);
    whole.push_str(text);
    let cut = whole.rfind(['\r', '\n']).map(|i| i + 1).unwrap_or(0);
    let tail = whole.split_off(cut);
    if tail.len() <= 1024 {
        *carry = tail;
    }
    Ok(whole
        .split('\r')
        .flat_map(|part| part.split('\n'))
        .filter_map(parse_ag_result_line)
        .collect())
}

fn parse_ag_result_line(line: &str) -> Option<HfpAgResult> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    if line == "OK" {
        return Some(HfpAgResult::Ok);
    }
    if line == "ERROR" {
        return Some(HfpAgResult::Error);
    }
    if line == "RING" {
        return Some(HfpAgResult::Ring);
    }
    if let Some(value) = line.strip_prefix("+BRSF:") {
        return parse_i32(value).map(|features| HfpAgResult::SupportedFeatures(features as u32));
    }
    if let Some(value) = line.strip_prefix("+CIND:") {
        return Some(parse_cind(value));
    }
    if let Some(value) = line.strip_prefix("+CIEV:") {
        let values = parse_number_list(value);
        if values.len() >= 2 {
            return Some(HfpAgResult::IndicatorUpdate {
                index: values[0] as u8,
                value: values[1],
            });
        }
    }
    if let Some(value) = line.strip_prefix("+CLIP:") {
        return extract_first_quoted(value).map(HfpAgResult::CallerId);
    }
    if let Some(value) = line.strip_prefix("+CLCC:") {
        // AT+CLCC (list current calls) response, parsed in full (Phase 4):
        // the entry feeds both the multi-call topology AND — for the active
        // or plain-incoming leg only — the same CallerId rescue pipeline as
        // +CLIP (see apply_result). An unparseable line is silently ignored.
        return parse_clcc_entry(value).map(HfpAgResult::CallListEntry);
    }
    if let Some(value) = line.strip_prefix("+CCWA:") {
        // Network call-waiting notification (armed by AT+CCWA=1): a second
        // caller is knocking while a call is active. Number rides the first
        // quoted field; a number-less line is ignored (never a phantom).
        return extract_first_quoted(value).map(HfpAgResult::CallWaitingNotification);
    }
    if let Some(value) = line.strip_prefix("+CHLD:") {
        // The AT+CHLD=? probe reply: "(0,1,1x,2,2x,3)" — the AG's supported
        // call-hold modes.
        let modes: Vec<String> = value
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        return Some(HfpAgResult::CallHoldModes(modes));
    }
    if let Some(value) = line.strip_prefix("+BCS:") {
        return parse_i32(value).map(|codec| HfpAgResult::SelectedCodec(codec as u8));
    }

    Some(HfpAgResult::Unknown(line.to_string()))
}

fn parse_cind(value: &str) -> HfpAgResult {
    if value.contains('"') {
        HfpAgResult::Indicators(extract_quoted_values(value))
    } else {
        HfpAgResult::IndicatorStatus(parse_number_list(value))
    }
}

fn parse_number_list(value: &str) -> Vec<i32> {
    value.split(',').filter_map(parse_i32).collect()
}

fn parse_i32(value: &str) -> Option<i32> {
    value.trim().parse::<i32>().ok()
}

fn extract_quoted_values(value: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find('"') {
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('"') else {
            break;
        };
        values.push(after_start[..end].to_string());
        rest = &after_start[end + 1..];
    }
    values
}

fn extract_first_quoted(value: &str) -> Option<String> {
    extract_quoted_values(value).into_iter().next()
}

/// Parse one `+CLCC:` body — `<idx>,<dir>,<stat>,<mode>,<mpty>[,"<num>",<type>]`.
/// The five leading integers are mandatory per 3GPP 27.007 §7.18; anything
/// short of that is malformed and dropped.
fn parse_clcc_entry(value: &str) -> Option<ClccEntry> {
    let unquoted = value.split('"').next().unwrap_or(value);
    let fields: Vec<i32> = parse_number_list(unquoted);
    if fields.len() < 5 {
        return None;
    }
    Some(ClccEntry {
        index: u8::try_from(fields[0]).ok()?,
        direction: u8::try_from(fields[1]).ok()?,
        status: u8::try_from(fields[2]).ok()?,
        mode: u8::try_from(fields[3]).ok()?,
        multiparty: fields[4] != 0,
        number: extract_first_quoted(value),
    })
}

fn codec_event(codec: u8) -> HfpEvent {
    match codec {
        HFP_CODEC_MSBC => HfpEvent::CodecSelected {
            codec: "mSBC".to_string(),
            sample_rate: 16000,
        },
        _ => HfpEvent::CodecSelected {
            codec: "CVSD".to_string(),
            sample_rate: 8000,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_service_level_and_call_control_commands() {
        assert_eq!(
            build_at_command(HfpAtCommand::SupportedFeatures {
                call_waiting: false
            }),
            b"AT+BRSF=693\r"
        );
        // Phase 4: the holdAndCallWaiting setting adds HF bit 1 (call
        // waiting / 3-way) — what makes the AT+CHLD=? probe spec-legal.
        assert_eq!(
            build_at_command(HfpAtCommand::SupportedFeatures { call_waiting: true }),
            b"AT+BRSF=695\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::EnableCallWaitingNotifications),
            b"AT+CCWA=1\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::CallHold(ChldAction::ReleaseAllHeld)),
            b"AT+CHLD=0\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::CallHold(ChldAction::ReleaseActiveAcceptOther)),
            b"AT+CHLD=1\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::CallHold(ChldAction::ReleaseSpecific(2))),
            b"AT+CHLD=12\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::CallHold(ChldAction::HoldActiveAcceptOther)),
            b"AT+CHLD=2\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::CallHold(ChldAction::PrivateConsult(1))),
            b"AT+CHLD=21\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::CallHold(ChldAction::Merge)),
            b"AT+CHLD=3\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::AvailableCodecs {
                wbs_supported: true
            }),
            b"AT+BAC=1,2\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::AvailableCodecs {
                wbs_supported: false
            }),
            b"AT+BAC=1\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::ActivateClip(true)),
            b"AT+CLIP=1\r"
        );
        assert_eq!(
            build_at_command(HfpAtCommand::EnableIndicatorUpdates(true)),
            b"AT+CMER=3,0,0,1\r"
        );
        assert_eq!(build_at_command(HfpAtCommand::Answer), b"ATA\r");
        assert_eq!(build_at_command(HfpAtCommand::RejectOrHangup), b"AT+CHUP\r");
    }

    #[test]
    fn parses_ag_results_and_notifications() {
        let results = parse_ag_results(
            b"\r\n+BRSF: 1536\r\nOK\r\n+CIND: (\"service\",(0,1)),(\"call\",(0,1)),(\"callsetup\",(0-3))\r\n+CIND: 1,0,1,0,0,0,0\r\n+CIEV: 2,1\r\n+CLIP: \"+15551234567\",145\r\n+BCS: 2\r\nRING\r\n",
        )
        .unwrap();

        assert!(results.contains(&HfpAgResult::SupportedFeatures(1536)));
        assert!(results.contains(&HfpAgResult::Ok));
        assert!(results.contains(&HfpAgResult::IndicatorStatus(vec![1, 0, 1, 0, 0, 0, 0])));
        assert!(results.contains(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }));
        assert!(results.contains(&HfpAgResult::CallerId("+15551234567".to_string())));
        assert!(results.contains(&HfpAgResult::SelectedCodec(HFP_CODEC_MSBC)));
        assert!(results.contains(&HfpAgResult::Ring));

        let indicators = results
            .iter()
            .find_map(|result| match result {
                HfpAgResult::Indicators(values) => Some(values),
                _ => None,
            })
            .unwrap();
        assert_eq!(indicators, &vec!["service", "call", "callsetup"]);
    }

    #[test]
    fn missing_cind_definitions_get_exactly_one_retry() {
        // Definitions lost on the wire: the pump asks once, then never loops.
        let mut state = HfpHandsFreeState::new();
        assert!(state.needs_indicator_definitions_retry());
        assert!(!state.needs_indicator_definitions_retry());
        // Definitions parsed (Android order — call FIRST): no retry, and the
        // mapping reflects the phone's real order, not the defaults.
        let mut ok = HfpHandsFreeState::new();
        ok.apply_result(&HfpAgResult::Indicators(vec![
            "call".to_string(),
            "callsetup".to_string(),
            "service".to_string(),
        ]));
        assert!(!ok.needs_indicator_definitions_retry());
        assert_eq!(ok.call_indicator_index, 1);
        assert_eq!(ok.callsetup_indicator_index, 2);
    }

    #[test]
    fn cind_definitions_split_across_frames_reassemble() {
        // Live incident 2026-07-14 (phantom answer): the +CIND=? DEFINITIONS
        // line fragmented at the mux MTU; per-frame parsing saw two garbage
        // halves, the default call/callsetup indices stood, and a phone with
        // Android's order (call FIRST) had every ringing CIEV misread as
        // CallAnswered. The carry buffer must reassemble the split line.
        let mut carry = String::new();
        let first = parse_ag_results_buffered(
            &mut carry,
            b"\r\n+CIND: (\"call\",(0,1)),(\"callsetup\",(0-3)),(\"serv",
        )
        .unwrap();
        // The fragment is HELD, not parsed as a half-line.
        assert!(first.is_empty(), "fragment must not parse: {first:?}");
        let second =
            parse_ag_results_buffered(&mut carry, b"ice\",(0,1)),(\"signal\",(0-5))\r\nOK\r\n")
                .unwrap();
        let indicators = second
            .iter()
            .find_map(|r| match r {
                HfpAgResult::Indicators(v) => Some(v.clone()),
                _ => None,
            })
            .expect("reassembled definitions line parses");
        assert_eq!(indicators, vec!["call", "callsetup", "service", "signal"]);
        assert!(second.contains(&HfpAgResult::Ok));
        // Applied to the state machine, the ANDROID order maps correctly:
        // CIEV 2,1 is callsetup (ringing), NEVER a phantom CallAnswered.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::Indicators(indicators));
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::IncomingCall]
        );
        assert!(!state.call_active());
    }

    #[test]
    fn carry_is_bounded_against_unterminated_streams() {
        let mut carry = String::new();
        let junk = vec![b'x'; 4096];
        let results = parse_ag_results_buffered(&mut carry, &junk).unwrap();
        assert!(results.is_empty());
        // An unterminated 4 KiB blob is dropped, not held forever.
        assert!(carry.is_empty());
        // Normal traffic keeps flowing afterwards.
        let ok = parse_ag_results_buffered(&mut carry, b"\r\nOK\r\n").unwrap();
        assert!(ok.contains(&HfpAgResult::Ok));
    }

    #[test]
    fn state_maps_indicators_to_call_events() {
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            vec![HfpEvent::IncomingCall]
        );
        assert!(state.incoming_call());
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
        assert!(state.call_active());
        assert!(!state.incoming_call());
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
        assert!(!state.call_active());
    }

    #[test]
    fn state_uses_ag_defined_indicator_order() {
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::Indicators(vec![
                "service".to_string(),
                "signal".to_string(),
                "callsetup".to_string(),
                "call".to_string(),
            ])),
            Vec::<HfpEvent>::new()
        );
        assert_eq!(state.callsetup_indicator_index(), 3);
        assert_eq!(state.call_indicator_index(), 4);

        // CIND? snapshot syncs state silently — no events fired even
        // when callsetup=1, so a phantom callsetup carry-over from a
        // recent missed call doesn't trigger auto-answer at SLC time.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorStatus(vec![1, 4, 1, 0])),
            Vec::<HfpEvent>::new()
        );
        assert!(state.incoming_call());
        // Subsequent +CIEV transitions still fire normal call events.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 4, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
        assert!(state.call_active());
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 4, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
    }

    #[test]
    fn cind_status_does_not_fire_phantom_call_events() {
        // Regression: an AG (Pixel observed) sometimes reports
        // callsetup=1 in its initial CIND? response when the HF
        // connects, even though no incoming call exists. The old
        // implementation emitted IncomingCall from the snapshot,
        // tripping auto-answer and leaving the dashboard stuck on
        // "Unknown Caller" with no SCO.
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorStatus(vec![1, 0, 1, 0, 4, 0, 5])),
            Vec::<HfpEvent>::new()
        );
        assert!(state.incoming_call());
        assert!(!state.call_active());
        // A real RING after SLC ready still fires events normally.
        assert_eq!(
            state.apply_result(&HfpAgResult::Ring),
            vec![HfpEvent::IncomingCall, HfpEvent::Ringing]
        );
    }

    #[test]
    fn cind_status_with_active_call_clears_incoming_flag() {
        // If the AG reports an already-active call at SLC time
        // (call=1), the snapshot is silent but we still want
        // incoming_call to be false so a subsequent CallTerminated
        // doesn't double up.
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorStatus(vec![1, 1, 0, 0, 4, 0, 5])),
            Vec::<HfpEvent>::new()
        );
        assert!(state.call_active());
        assert!(!state.incoming_call());
    }

    #[test]
    fn new_incoming_setup_while_call_active_synthesizes_missed_terminate() {
        // Regression: the previous call's `+CIEV: call,0` can be lost or
        // coalesced (back-to-back / call-waiting calls, or an SCO drop whose
        // CallTerminated never drove the HFP state machine). call_active then
        // stays stuck true, so the next call's `call,1` is swallowed by the
        // edge guard in update_call_state and NO CallAnswered fires. The
        // consumer clears per-call conversation state only on CallAnswered,
        // so the prior call's conversation leaks into the new call. A fresh
        // incoming-call setup must synthesize the missed terminate so the
        // upcoming answer is a genuine 0->1 edge.
        let mut state = HfpHandsFreeState::new();
        // Call A is answered (call=1, default call indicator index 2).
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
        assert!(state.call_active());
        // Call A's `call,0` never arrives. Call B rings: callsetup=1
        // (default callsetup indicator index 3). The stale active call must
        // be torn down first so the next answer is a clean edge.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            vec![HfpEvent::CallTerminated, HfpEvent::IncomingCall]
        );
        assert!(!state.call_active());
        assert!(state.incoming_call());
        // Call B is answered — now a genuine 0->1 edge, so CallAnswered fires
        // and the consumer runs its full per-call reset.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
        assert!(state.call_active());
    }

    #[test]
    fn normal_incoming_setup_without_active_call_is_unchanged() {
        // Happy path must be untouched: a callsetup=1 with no active call
        // emits only IncomingCall, with no synthesized terminate.
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            vec![HfpEvent::IncomingCall]
        );
        assert!(!state.call_active());
        assert!(state.incoming_call());
    }

    // ── AOK-CTRL-001: callsetup-drop is a HELD verdict, never a guess ──────
    // Live bug (2026-07-13): the phone sent `+CIEV: callsetup,0` BEFORE
    // `+CIEV: call,1` when the auto-answer connected; the old code read the
    // drop as "ring abandoned" → CallTerminated, the plugin killed its call
    // session (outcome "missed", 0s) and dropped every audio frame of a call
    // that was actually up — the receptionist heard nothing for the rest of
    // the call. The drop must wait for the `call` indicator to decide.

    #[test]
    fn callsetup_drop_before_call_up_is_the_answer_not_a_terminate() {
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            vec![HfpEvent::IncomingCall]
        );
        // The ambiguous drop: NO terminate may fire here.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 }),
            Vec::<HfpEvent>::new()
        );
        // The call indicator resolves it as the ANSWER.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
        assert!(state.call_active());
        // And the eventual hangup still terminates exactly once.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
    }

    #[test]
    fn abandoned_ring_resolves_terminated_via_the_cind_snapshot() {
        // Caller gave up before the answer: callsetup drops, `call` never
        // rises. The ACL-keepalive AT+CIND? poll (~8s later) shows call=0
        // and resolves the held verdict as the abandon.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 }),
            Vec::<HfpEvent>::new()
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorStatus(vec![1, 0, 0])),
            vec![HfpEvent::CallTerminated]
        );
        assert!(!state.call_active());
        // The verdict is consumed — the next snapshot stays silent.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorStatus(vec![1, 0, 0])),
            Vec::<HfpEvent>::new()
        );
    }

    // ── Phase 2: OUTBOUND (mobile-originated) call lifecycle ───────────────

    #[test]
    fn outbound_dial_command_is_sanitized_atd() {
        assert_eq!(
            build_at_command(HfpAtCommand::Dial("+61 491 570-156".to_string())),
            b"ATD+61491570156;\r"
        );
        // A `+` anywhere but the front (or any other junk) is stripped — no
        // way to smuggle AT syntax through the number.
        assert_eq!(
            build_at_command(HfpAtCommand::Dial("04;DT99\r+21".to_string())),
            b"ATD049921;\r"
        );
    }

    #[test]
    fn outbound_setup_alert_answer_hangup_lifecycle() {
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 2 }),
            vec![HfpEvent::OutgoingDialing]
        );
        assert!(!state.incoming_call(), "MO setup is never an incoming call");
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 3 }),
            vec![HfpEvent::Ringing]
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
        // Setup phase over; a later callsetup,0 echo must stay silent.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 }),
            Vec::<HfpEvent>::new()
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
    }

    #[test]
    fn outbound_no_answer_resolves_terminated_via_the_held_verdict() {
        // Remote never picked up: callsetup 2 → 3 → 0 with `call` never
        // rising. The drop is the SAME ambiguous edge as the MT ring — the
        // verdict holds until the call indicator (snapshot here) decides.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 2 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 3 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 }),
            Vec::<HfpEvent>::new()
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorStatus(vec![1, 0, 0])),
            vec![HfpEvent::CallTerminated]
        );
        assert!(!state.call_active());
    }

    #[test]
    fn outbound_answer_race_callsetup_drop_before_call_up() {
        // Same AG quirk as the MT answer (observed live 2026-07-13):
        // callsetup,0 lands BEFORE call,1 on the remote pickup. The held
        // verdict must resolve as the ANSWER, never a failed attempt.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 2 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 }),
            Vec::<HfpEvent>::new()
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
    }

    #[test]
    fn outbound_straight_to_alerting_is_still_tracked() {
        // Some AGs skip callsetup=2 and report 3 directly — the attempt must
        // still be tracked so its callsetup,0 resolves honestly.
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 3 }),
            vec![HfpEvent::Ringing]
        );
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
    }

    #[test]
    fn dialing_clears_a_stale_held_verdict_from_the_previous_ring() {
        // Live incident 2026-07-14 (silent callback): a missed inbound ring
        // left its callsetup-drop verdict HELD (the session was closed via
        // the SCO-teardown path instead); the callback's ATD echo
        // (callsetup,2) then discharged the stale verdict as CallTerminated
        // and killed the fresh dial session. Sending Dial must clear the
        // held state — the dialer only dials when it considers the line idle.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 }),
            Vec::<HfpEvent>::new(),
            "verdict held"
        );
        state.mark_command_sent(&HfpAtCommand::Dial("0491570156".to_string()));
        // The ATD echo now announces the outbound setup — NO stale terminate.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 2 }),
            vec![HfpEvent::OutgoingDialing]
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
    }

    #[test]
    fn fresh_outbound_setup_resolves_a_stranded_prior_call_first() {
        // Mirror of the fresh-incoming boundary rule: a lost call,0 must not
        // let the previous call's session leak into the new outbound one.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 2 }),
            vec![HfpEvent::CallTerminated, HfpEvent::OutgoingDialing]
        );
    }

    #[test]
    fn abandoned_ring_resolves_terminated_via_an_explicit_call_zero_ciev() {
        // Some AGs confirm the abandon with `+CIEV: call,0` even though the
        // call never went active — that explicit word resolves the verdict.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
    }

    #[test]
    fn lost_call_edge_resolves_answered_via_the_cind_snapshot() {
        // The `+CIEV: call,1` was lost entirely; the keepalive snapshot shows
        // call=1 while the verdict is held → the consumer is owed exactly one
        // CallAnswered (the snapshot is otherwise silent by design).
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorStatus(vec![1, 1, 0])),
            vec![HfpEvent::CallAnswered]
        );
        assert!(state.call_active());
        // A late duplicate `call,1` CIEV stays silent (edge guard).
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            Vec::<HfpEvent>::new()
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
    }

    #[test]
    fn a_new_ring_while_the_verdict_is_held_terminates_the_prior_ring() {
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 });
        // Nothing ever answered ring A; ring B starting is its boundary.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            vec![HfpEvent::CallTerminated, HfpEvent::IncomingCall]
        );
        assert!(state.incoming_call());
    }

    #[test]
    fn a_live_ring_line_drops_the_held_verdict() {
        // RING still arriving after a callsetup blip = the ring never ended;
        // the answer that follows must fire cleanly with no phantom terminate.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 });
        assert_eq!(
            state.apply_result(&HfpAgResult::Ring),
            vec![HfpEvent::IncomingCall, HfpEvent::Ringing]
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
        // The dropped verdict must not resurface at the eventual hangup.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
    }

    /// AT+CLCC rescue (2026-07-13): the +CLCC response's quoted number feeds
    /// the SAME CallerId pipeline as +CLIP — the deterministic caller-id
    /// source when instant auto-answer races the ringing-phase +CLIP away.
    #[test]
    fn clcc_response_feeds_the_caller_id_pipeline() {
        let results =
            parse_ag_results(b"\r\n+CLCC: 1,1,0,0,0,\"0491570156\",129\r\nOK\r\n").unwrap();
        assert_eq!(
            results[0],
            HfpAgResult::CallListEntry(ClccEntry {
                index: 1,
                direction: 1,
                status: 0,
                mode: 0,
                multiparty: false,
                number: Some("0491570156".to_string()),
            })
        );
        // Applied to the state machine, the ACTIVE entry still feeds the
        // CallerId rescue pipeline exactly as before.
        let mut state = HfpHandsFreeState::new();
        let events = state.apply_result(&results[0]);
        assert!(events.contains(&HfpEvent::CallerId("0491570156".to_string())));
        // A number-less +CLCC (withheld caller id) still parses as topology
        // but never mints a phantom CallerId.
        let results = parse_ag_results(b"\r\n+CLCC: 1,1,0,0,0\r\n").unwrap();
        assert_eq!(results.len(), 1);
        let events = state.apply_result(&results[0]);
        assert!(!events.iter().any(|e| matches!(e, HfpEvent::CallerId(_))));
        // And the command renders per spec.
        assert_eq!(
            build_at_command(HfpAtCommand::ListCurrentCalls),
            b"AT+CLCC\r".to_vec()
        );
    }

    /// Phase 4 regression (the neighbour-number class): with a held or
    /// waiting second call in the CLCC list, only the ACTIVE/incoming leg's
    /// number may feed the CallerId pipeline — the old first-quoted parse
    /// let the LAST line win and overwrite the active caller's id.
    #[test]
    fn clcc_held_and_waiting_entries_never_feed_caller_id() {
        let results = parse_ag_results(
            b"\r\n+CLCC: 1,1,0,0,0,\"0491570156\",129\r\n+CLCC: 2,1,5,0,0,\"0491570157\",129\r\n+CLCC: 3,1,1,0,0,\"0491570158\",129\r\nOK\r\n",
        )
        .unwrap();
        let mut state = HfpHandsFreeState::new();
        let mut caller_ids = Vec::new();
        let mut entries = 0;
        for result in &results {
            for event in state.apply_result(result) {
                match event {
                    HfpEvent::CallerId(n) => caller_ids.push(n),
                    HfpEvent::CallListEntry(_) => entries += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(caller_ids, vec!["0491570156".to_string()]);
        assert_eq!(entries, 3, "all three legs surface as topology entries");
    }

    // ── Phase 4: call-waiting capability negotiation ────────────────────

    fn negotiated_state() -> HfpHandsFreeState {
        let mut state = HfpHandsFreeState::new();
        state.set_call_waiting_enabled(true);
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFF));
        state.apply_result(&HfpAgResult::CallHoldModes(vec![
            "0".into(),
            "1".into(),
            "1x".into(),
            "2".into(),
            "2x".into(),
            "3".into(),
        ]));
        state.note_ccwa_accepted();
        assert!(state.three_way_negotiated());
        state
    }

    #[test]
    fn chld_probe_reply_parses_modes() {
        let results = parse_ag_results(b"\r\n+CHLD: (0,1,1x,2,2x,3)\r\nOK\r\n").unwrap();
        assert!(results.contains(&HfpAgResult::CallHoldModes(vec![
            "0".into(),
            "1".into(),
            "1x".into(),
            "2".into(),
            "2x".into(),
            "3".into()
        ])));
    }

    #[test]
    fn capability_gate_requires_every_leg() {
        // Setting off → gate closed no matter what the AG says.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFF));
        state.apply_result(&HfpAgResult::CallHoldModes(vec!["1".into(), "2".into()]));
        state.note_ccwa_accepted();
        assert!(!state.three_way_negotiated());
        // AG without the three-way bit → closed.
        let mut state = HfpHandsFreeState::new();
        state.set_call_waiting_enabled(true);
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFE));
        state.apply_result(&HfpAgResult::CallHoldModes(vec!["1".into(), "2".into()]));
        state.note_ccwa_accepted();
        assert!(!state.three_way_negotiated());
        // Missing plain mode 2 → closed (indexed-only lists don't count).
        let mut state = HfpHandsFreeState::new();
        state.set_call_waiting_enabled(true);
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFF));
        state.apply_result(&HfpAgResult::CallHoldModes(vec!["1".into(), "2x".into()]));
        state.note_ccwa_accepted();
        assert!(!state.three_way_negotiated());
        // No CCWA ack → closed.
        let mut state = HfpHandsFreeState::new();
        state.set_call_waiting_enabled(true);
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFF));
        state.apply_result(&HfpAgResult::CallHoldModes(vec!["1".into(), "2".into()]));
        assert!(!state.three_way_negotiated());
        // All four legs → open.
        assert!(negotiated_state().three_way_negotiated());
    }

    #[test]
    fn probes_skip_unless_both_sides_support_three_way() {
        // Feature off: both probes skip regardless of the AG.
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFF));
        assert!(state.should_skip_slc_command(&HfpAtCommand::RetrieveCallHoldSupport));
        assert!(state.should_skip_slc_command(&HfpAtCommand::EnableCallWaitingNotifications));
        // Feature on but AG lacks three-way: still skipped (sending
        // AT+CHLD=? there is out of spec — the original fatal-ERROR bug).
        let mut state = HfpHandsFreeState::new();
        state.set_call_waiting_enabled(true);
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFE));
        assert!(state.should_skip_slc_command(&HfpAtCommand::RetrieveCallHoldSupport));
        // Both sides on: sent.
        let mut state = HfpHandsFreeState::new();
        state.set_call_waiting_enabled(true);
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFF));
        assert!(!state.should_skip_slc_command(&HfpAtCommand::RetrieveCallHoldSupport));
        assert!(!state.should_skip_slc_command(&HfpAtCommand::EnableCallWaitingNotifications));
        // Other commands never skip.
        assert!(!state.should_skip_slc_command(&HfpAtCommand::ActivateClip(true)));
    }

    #[test]
    fn probe_errors_are_non_fatal_capability_off() {
        let mut state = HfpHandsFreeState::new();
        state.set_call_waiting_enabled(true);
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFF));
        assert!(state.note_slc_probe_error(&HfpAtCommand::RetrieveCallHoldSupport));
        assert!(state.note_slc_probe_error(&HfpAtCommand::EnableCallWaitingNotifications));
        assert!(!state.three_way_negotiated());
        // Everything else keeps the fatal policy.
        assert!(!state.note_slc_probe_error(&HfpAtCommand::RetrieveIndicators));
        assert!(!state.note_slc_probe_error(&HfpAtCommand::ActivateClip(true)));
    }

    // ── Phase 4: waiting-episode state machine ──────────────────────────

    #[test]
    fn ccwa_during_active_call_starts_one_waiting_episode() {
        let mut state = negotiated_state();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 });
        // First +CCWA opens the episode with the number.
        assert_eq!(
            state.apply_result(&HfpAgResult::CallWaitingNotification(
                "0491570157".to_string()
            )),
            vec![HfpEvent::CallWaiting(Some("0491570157".to_string()))]
        );
        // The callsetup=1 edge that rides with it must NOT kill the active
        // call (the old lost-call,0 recovery) nor double-announce.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            Vec::<HfpEvent>::new()
        );
        assert!(state.call_active(), "active call survives the knock");
        // +CCWA repeats like RING — silent.
        assert_eq!(
            state.apply_result(&HfpAgResult::CallWaitingNotification(
                "0491570157".to_string()
            )),
            Vec::<HfpEvent>::new()
        );
        // The waiting caller gives up: episode ends, active call untouched.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 }),
            vec![HfpEvent::CallWaitingEnded]
        );
        assert!(state.call_active());
        // The active call still terminates exactly once at hangup.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
    }

    #[test]
    fn callsetup_first_waiting_episode_upgrades_with_the_ccwa_number() {
        let mut state = negotiated_state();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 });
        // callsetup=1 lands before any +CCWA: anonymous episode start.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            vec![HfpEvent::CallWaiting(None)]
        );
        // The +CCWA that follows names the caller — one upgrade, then silent.
        assert_eq!(
            state.apply_result(&HfpAgResult::CallWaitingNotification(
                "0491570157".to_string()
            )),
            vec![HfpEvent::CallWaiting(Some("0491570157".to_string()))]
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::CallWaitingNotification(
                "0491570157".to_string()
            )),
            Vec::<HfpEvent>::new()
        );
    }

    #[test]
    fn active_call_ending_promotes_the_waiting_caller_to_a_normal_ring() {
        let mut state = negotiated_state();
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 });
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 });
        state.apply_result(&HfpAgResult::CallWaitingNotification(
            "0491570157".to_string(),
        ));
        // Caller A hangs up while B is still knocking: B becomes a normal
        // incoming ring (callsetup is already 1, so no fresh edge will come).
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 0 }),
            vec![
                HfpEvent::CallWaitingEnded,
                HfpEvent::CallTerminated,
                HfpEvent::IncomingCall,
            ]
        );
        assert!(state.incoming_call());
        assert!(!state.call_active());
        // B is answered like any fresh call.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 }),
            vec![HfpEvent::CallAnswered]
        );
    }

    /// Phase 4 hold queue (live 2026-07-15): with a caller HELD, the
    /// promotion must NOT fire — auto-answering the knocker would queue-jump
    /// them past the held caller. The episode stays alive for the plugin's
    /// FIFO cascade (CHLD=2 prefers the waiting leg), and a re-presenting
    /// RING is swallowed for the same reason.
    #[test]
    fn held_call_blocks_the_waiting_ring_promotion() {
        let mut state = negotiated_state();
        state.apply_result(&HfpAgResult::Indicators(vec![
            "call".to_string(),
            "callsetup".to_string(),
            "callheld".to_string(),
        ]));
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 1, value: 1 }); // A active
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }); // B held + active
        state.apply_result(&HfpAgResult::CallWaitingNotification(
            "0491570157".to_string(),
        ));
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 2 }); // held only
                                                                                  // A ends (this Pixel reports call=0 even with B still held): the
                                                                                  // honest termination, NO promotion.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 1, value: 0 }),
            vec![HfpEvent::CallTerminated]
        );
        assert!(!state.incoming_call());
        // The knocker's re-presenting RING is swallowed too — an ATA here
        // would answer them ahead of the held caller.
        assert_eq!(
            state.apply_result(&HfpAgResult::Ring),
            Vec::<HfpEvent>::new()
        );
        // Once nothing is held any more, a ring mints normally again.
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 });
        assert_eq!(
            state.apply_result(&HfpAgResult::Ring),
            vec![HfpEvent::IncomingCall, HfpEvent::Ringing]
        );
    }

    #[test]
    fn without_negotiation_a_second_setup_keeps_the_legacy_recovery() {
        // Setting ON but the AG never negotiated (e.g. CHLD probe refused):
        // callsetup=1 while active must keep the lost-call,0 recovery —
        // exactly the pre-Phase-4 behaviour.
        let mut state = HfpHandsFreeState::new();
        state.set_call_waiting_enabled(true);
        state.apply_result(&HfpAgResult::SupportedFeatures(0x0FFF));
        state.apply_result(&HfpAgResult::IndicatorUpdate { index: 2, value: 1 });
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            vec![HfpEvent::CallTerminated, HfpEvent::IncomingCall]
        );
        // A +CCWA arriving with no negotiated episode support (call not
        // active here) stays silent.
        assert_eq!(
            state.apply_result(&HfpAgResult::CallWaitingNotification("0491570157".into())),
            Vec::<HfpEvent>::new()
        );
    }

    #[test]
    fn callheld_indicator_transitions_surface_and_dedupe() {
        let mut state = HfpHandsFreeState::new();
        state.apply_result(&HfpAgResult::Indicators(vec![
            "call".to_string(),
            "callsetup".to_string(),
            "callheld".to_string(),
        ]));
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            vec![HfpEvent::CallHeld(1)]
        );
        // Repeat value — silent.
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 1 }),
            Vec::<HfpEvent>::new()
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::IndicatorUpdate { index: 3, value: 0 }),
            vec![HfpEvent::CallHeld(0)]
        );
        // Without a callheld definition the index falls through to nothing
        // (default state has no callheld index).
        let mut plain = HfpHandsFreeState::new();
        assert_eq!(
            plain.apply_result(&HfpAgResult::IndicatorUpdate { index: 9, value: 1 }),
            Vec::<HfpEvent>::new()
        );
    }

    #[test]
    fn state_maps_ring_clip_and_codec_events() {
        let mut state = HfpHandsFreeState::new();
        assert_eq!(
            state.apply_result(&HfpAgResult::Ring),
            vec![HfpEvent::IncomingCall, HfpEvent::Ringing]
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::CallerId("555".to_string())),
            vec![HfpEvent::CallerId("555".to_string())]
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::SelectedCodec(HFP_CODEC_MSBC)),
            vec![HfpEvent::CodecSelected {
                codec: "mSBC".to_string(),
                sample_rate: 16000,
            }]
        );
        assert_eq!(
            state.apply_result(&HfpAgResult::SelectedCodec(HFP_CODEC_CVSD)),
            vec![HfpEvent::CodecSelected {
                codec: "CVSD".to_string(),
                sample_rate: 8000,
            }]
        );
    }

    #[test]
    fn fuzz_parse_ag_results_does_not_panic_on_random_bytes() {
        // HFP AT commands are a string format with known-prefix
        // tokens; random bytes (and invalid UTF-8) must never panic
        // the parser. parse_ag_results returns Result so it's
        // allowed to Err.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x4846_5046_5550_5346);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 256) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_ag_results(&buf);
        }
    }
}
