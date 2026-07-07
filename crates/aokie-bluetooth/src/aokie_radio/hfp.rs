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
    ConfirmCodec(u8),
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
    Ringing,
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
        if let Some(call) = indicator_status_value(values, self.call_indicator_index) {
            let active = *call != 0;
            self.call_active = active;
            if active {
                // An already-active call at SLC time — make sure
                // incoming_call doesn't shadow it.
                self.incoming_call = false;
            }
        }
        if let Some(callsetup) = indicator_status_value(values, self.callsetup_indicator_index) {
            // Only callsetup=1 (incoming) needs to be reflected so we
            // don't auto-answer phantom rings. Outgoing (2) and alerting
            // (3) aren't relevant for the HF's snapshot view.
            self.incoming_call = *callsetup == 1 && !self.call_active;
        }
        Vec::new()
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
                if self.incoming_call && !self.call_active {
                    self.incoming_call = false;
                    vec![HfpEvent::CallTerminated]
                } else {
                    Vec::new()
                }
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
                if self.call_active {
                    self.call_active = false;
                    events.push(HfpEvent::CallTerminated);
                }
                self.incoming_call = true;
                events.push(HfpEvent::IncomingCall);
                events
            }
            3 => vec![HfpEvent::Ringing],
            _ => Vec::new(),
        }
    }

    fn update_call_state(&mut self, active: bool) -> Vec<HfpEvent> {
        if active == self.call_active {
            return Vec::new();
        }

        self.call_active = active;
        self.incoming_call = false;
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
        HfpAtCommand::ConfirmCodec(codec) => format!("AT+BCS={}\r", codec),
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
