pub const HF_FEATURE_EC_NR: u32 = 1 << 0;
pub const HF_FEATURE_CLI_PRESENTATION: u32 = 1 << 2;
pub const HF_FEATURE_REMOTE_VOLUME: u32 = 1 << 4;
pub const HF_FEATURE_ENHANCED_CALL_STATUS: u32 = 1 << 5;
pub const HF_FEATURE_CODEC_NEGOTIATION: u32 = 1 << 7;
pub const HF_FEATURE_HF_INDICATORS: u32 = 1 << 8;
pub const HF_FEATURE_ESCO_S4: u32 = 1 << 9;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfpAtCommand {
    SupportedFeatures,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfpAgResult {
    Ok,
    Error,
    Ring,
    SupportedFeatures(u32),
    Indicators(Vec<String>),
    IndicatorStatus(Vec<i32>),
    IndicatorUpdate { index: u8, value: i32 },
    CallerId(String),
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

    pub fn initial_service_level_commands(wbs_supported: bool) -> Vec<HfpAtCommand> {
        vec![
            HfpAtCommand::SupportedFeatures,
            HfpAtCommand::AvailableCodecs { wbs_supported },
            HfpAtCommand::RetrieveIndicators,
            HfpAtCommand::RetrieveIndicatorStatus,
            HfpAtCommand::RetrieveCallHoldSupport,
            HfpAtCommand::ActivateClip(true),
            HfpAtCommand::EnableIndicatorUpdates(true),
            HfpAtCommand::EnableAllIndicatorStatusUpdates(true),
            HfpAtCommand::DisableNoiseReduction,
        ]
    }

    pub fn apply_result(&mut self, result: &HfpAgResult) -> Vec<HfpEvent> {
        match result {
            HfpAgResult::Ok => {
                self.supported_features_exchanged = true;
                Vec::new()
            }
            HfpAgResult::Ring => {
                self.incoming_call = true;
                // A live RING while a terminate verdict was held means the
                // ring never actually ended — drop the held verdict.
                self.terminate_pending = false;
                vec![HfpEvent::IncomingCall, HfpEvent::Ringing]
            }
            HfpAgResult::Indicators(indicators) => {
                self.apply_indicator_definitions(indicators);
                Vec::new()
            }
            HfpAgResult::IndicatorUpdate { index, value } => self.apply_indicator(*index, *value),
            HfpAgResult::IndicatorStatus(values) => self.apply_indicator_status(values),
            HfpAgResult::CallerId(number) => vec![HfpEvent::CallerId(number.clone())],
            HfpAgResult::SelectedCodec(codec) => {
                self.selected_codec = Some(*codec);
                vec![codec_event(*codec)]
            }
            HfpAgResult::Error | HfpAgResult::SupportedFeatures(_) | HfpAgResult::Unknown(_) => {
                Vec::new()
            }
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
        Some(HfpEvent::ServiceLevelConnectionReady)
    }

    fn apply_indicator_definitions(&mut self, indicators: &[String]) {
        for (index, name) in indicators.iter().enumerate() {
            let hfp_index = (index + 1) as u8;
            if name.eq_ignore_ascii_case("call") {
                self.call_indicator_index = hfp_index;
            } else if name.eq_ignore_ascii_case("callsetup") {
                self.callsetup_indicator_index = hfp_index;
            }
        }
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
        }
        events
    }

    fn apply_indicator(&mut self, index: u8, value: i32) -> Vec<HfpEvent> {
        if index == self.call_indicator_index {
            self.update_call_state(value != 0)
        } else if index == self.callsetup_indicator_index {
            self.update_callsetup(value)
        } else {
            Vec::new()
        }
    }

    fn update_callsetup(&mut self, value: i32) -> Vec<HfpEvent> {
        match value {
            0 => {
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
        HfpAtCommand::SupportedFeatures => format!("AT+BRSF={}\r", AOKIE_HF_SUPPORTED_FEATURES),
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
    let text =
        std::str::from_utf8(payload).map_err(|err| format!("HFP payload is not UTF-8: {err}"))?;
    Ok(text
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
        // AT+CLCC (list current calls) response — the call's number rides in
        // the first quoted field. Feeds the SAME CallerId pipeline as +CLIP:
        // this is the rescue path when instant auto-answer races +CLIP out of
        // existence (observed live 2026-07-13: several calls ended with no
        // caller id at all). A number-less +CLCC line is silently ignored.
        return extract_first_quoted(value).map(HfpAgResult::CallerId);
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
            build_at_command(HfpAtCommand::SupportedFeatures),
            b"AT+BRSF=693\r"
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
        assert!(results.contains(&HfpAgResult::CallerId("0491570156".to_string())));
        // A number-less +CLCC (withheld caller id) parses to nothing —
        // never an Unknown-noise result, never a phantom empty CallerId.
        let results = parse_ag_results(b"\r\n+CLCC: 1,1,0,0,0\r\n").unwrap();
        assert_eq!(results, Vec::<HfpAgResult>::new());
        // And the command renders per spec.
        assert_eq!(build_at_command(HfpAtCommand::ListCurrentCalls), b"AT+CLCC\r".to_vec());
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
