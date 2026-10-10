//! Fixed lines said in OAIY's voice: the realtime start's speak mode.
//!
//! On the OAIY route Aokie's own TTS is normally absent, yet some lines are
//! Aokie's to say, not OAIY's agent's: the screen / blocked message before a
//! screened hang-up, the hold and queue announcements, and the apology when
//! OAIY's call session fails. OAIY Desktop says such a line when asked with a
//! `formlogic.realtime.start` whose `mode` is `"speak"` and whose `greeting`
//! is the line: it says it once, in the call voice, and nothing else happens
//! (no agent, the caller is not listened to, it is not a live call in OAIY).
//! The item's `output_item_done` says it has been said; then the session is
//! stopped.
//!
//! [`say_in_oaiy_voice`] runs one such session to its end on the radio
//! thread, as `tts_speak` runs Aokie's own synthesis: it paces the PCM into
//! the SCO link, drains (and discards) the caller's audio meanwhile, honours
//! an urgent hangup/reject, and always returns within its deadlines.
//! [`say_fixed_line`] tries OAIY's voice first and falls back to Aokie's own
//! TTS where it is present; off the OAIY route it is exactly the old local
//! speech.

#[allow(unused_imports)]
use super::*;

/// How long a line may take to start sounding (session connect, ready,
/// begin, first PCM) before it counts as not said.
#[cfg(feature = "voice")]
pub(super) const OAIY_SAY_FIRST_AUDIO: Duration = Duration::from_secs(5);

/// The failure apology's shorter wait: OAIY's own session just failed, so a
/// speak session may fail too, and the caller is waiting in silence.
#[cfg(feature = "voice")]
pub(super) const OAIY_APOLOGY_FIRST_AUDIO: Duration = Duration::from_millis(2_500);

/// No fixed line legitimately plays this long.
#[cfg(feature = "voice")]
const OAIY_SAY_MAX: Duration = Duration::from_secs(30);

/// Where a fixed line on a call is said.
#[cfg(all(aokie_radio, feature = "voice"))]
#[derive(Clone, Copy)]
pub(super) struct LineVoice<'a> {
    /// The OAIY route's realtime config, whose speak mode says the line in
    /// OAIY's voice. `None` off the OAIY route (and when its config failed).
    pub(super) oaiy: Option<&'a RealtimeRuntimeConfig>,
    /// Whether Aokie's own TTS may say it (when OAIY's voice is not used or
    /// could not). Always true off the OAIY route, as before.
    pub(super) own_tts: bool,
}

#[cfg(all(aokie_radio, feature = "voice"))]
impl<'a> LineVoice<'a> {
    /// Off the OAIY route: Aokie's own TTS, exactly as before.
    pub(super) fn own() -> Self {
        Self {
            oaiy: None,
            own_tts: true,
        }
    }

    /// The route's voice for fixed lines. On the OAIY route OAIY's speak
    /// mode first, then Aokie's own TTS only when its preflight found it.
    pub(super) fn for_route(
        oaiy_route: bool,
        realtime_config: Option<&'a RealtimeRuntimeConfig>,
        own_tts_available: bool,
    ) -> Self {
        if oaiy_route {
            Self {
                oaiy: realtime_config,
                own_tts: own_tts_available,
            }
        } else {
            Self::own()
        }
    }

    /// Whether any voice can say a line.
    pub(super) fn can_speak(&self) -> bool {
        self.oaiy.is_some() || self.own_tts
    }
}

/// What came of one line in OAIY's voice.
#[cfg(feature = "voice")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OaiySaid {
    /// Audio sent toward the caller.
    pub(super) dur: Duration,
    /// An urgent control (hangup/reject) cut it; the probe holds the action.
    pub(super) cancelled: bool,
    /// Why nothing (or not all) was said.
    pub(super) error: Option<String>,
}

#[cfg(feature = "voice")]
impl OaiySaid {
    /// Said, cut by the operator, or partly said: never repeat it in
    /// another voice (the caller would hear the line twice).
    pub(super) fn settled(&self) -> bool {
        self.cancelled || self.dur > Duration::ZERO
    }
}

/// The `formlogic.realtime.start` of a speak-mode session: the line as the
/// greeting, no brief, no tools, nothing about the caller.
#[cfg(all(aokie_radio, feature = "voice"))]
pub(super) fn oaiy_say_config(
    config: &RealtimeRuntimeConfig,
    call_id: &str,
    generation: u64,
    line: &str,
) -> crate::realtime_voice::SessionConfig {
    crate::realtime_voice::SessionConfig {
        endpoint: config.endpoint.clone(),
        call_id: call_id.to_string(),
        generation,
        expected_destination: config.destination.clone(),
        instructions: String::new(),
        greeting: line.to_string(),
        voice: Some(config.voice.clone()),
        model: None,
        turn_detection: config.turn_detection,
        max_output_tokens: config.max_output_tokens,
        allow_business_lookup: false,
        allow_request_appointment: false,
        allow_finish_call: false,
        call: crate::realtime_voice::CallFacts::default(),
        speak_only: true,
        allow_transfer: false,
        resume: None,
    }
}

/// Each speak session gets its own generation, so nothing from an earlier
/// session (or the call's own) can pass for its audio.
#[cfg(feature = "voice")]
fn next_say_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1 << 40);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Say `line` once in OAIY's voice on the call's SCO link, and return when
/// it has been said (the item is done and its audio played out), when it
/// could not be (no first audio within `first_audio_within`, the session
/// failed, the link stalled), or when `ctl` reports an urgent control.
/// Blocks the radio thread for the line, as `tts_speak` does.
#[cfg(all(aokie_radio, feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn say_in_oaiy_voice(
    bt: &mut dyn crate::backend::RadioBackend,
    config: &RealtimeRuntimeConfig,
    call_id: &str,
    line: &str,
    sample_rate: u16,
    first_audio_within: Duration,
    mut ctl: Option<&mut ControlProbe<'_>>,
    egress_hold: Option<Instant>,
) -> OaiySaid {
    let mut said = OaiySaid {
        dur: Duration::ZERO,
        cancelled: false,
        error: None,
    };
    if sample_rate == 0 || line.trim().is_empty() {
        said.error = Some("no call audio to say it on".into());
        return said;
    }
    let sr = u32::from(sample_rate);
    let session = match crate::realtime_voice::RealtimeVoiceSession::spawn(oaiy_say_config(
        config,
        call_id,
        next_say_generation(),
        line.trim(),
    )) {
        Ok(session) => session,
        Err(error) => {
            said.error = Some(error);
            return said;
        }
    };
    let mut resampler =
        crate::realtime_voice::StreamingResampler::new(crate::realtime_voice::WIRE_SAMPLE_RATE, sr);
    let mut pacer = crate::realtime_voice::OutputPacer::new(sr);
    let started = Instant::now();
    let mut begun = false;
    let mut item: Option<String> = None;
    let mut item_done = false;
    let mut sent_samples: u64 = 0;
    let mut first_sent: Option<Instant> = None;
    // Once the line is complete, a stream that closes only ends the events;
    // the queued audio still plays out.
    let mut events_over = false;
    let outcome: Result<(), String> = 'say: loop {
        let now = Instant::now();
        if let Some(probe) = ctl.as_deref_mut() {
            if probe.poll() {
                said.cancelled = true;
                break 'say Ok(());
            }
        }
        // The caller is not listened to: keep the SCO receive side drained.
        while AudioLink::try_recv_audio(bt).is_some() {}
        while !events_over {
            let event = match session.try_recv() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(_) if item_done => {
                    events_over = true;
                    break;
                }
                Err(error) => break 'say Err(error),
            };
            use crate::realtime_voice::RealtimeEventKind as K;
            match event.kind {
                K::Ready { .. } => {
                    if !begun {
                        if let Err(error) = session.begin(None, None) {
                            break 'say Err(error);
                        }
                        begun = true;
                    }
                }
                K::OutputItemStarted { item_id } => {
                    if item.is_some() {
                        // One line, one item: anything more is not ours.
                        continue;
                    }
                    if let Err(error) = pacer.start_item(&item_id, now) {
                        break 'say Err(error);
                    }
                    item = Some(item_id);
                }
                K::OutputPcm { item_id, samples } => {
                    if item.as_deref() != Some(item_id.as_str()) {
                        continue;
                    }
                    let converted = resampler.process(&samples);
                    if let Err(error) = pacer.push(&item_id, &converted) {
                        break 'say Err(error.to_string());
                    }
                }
                K::OutputItemDone { item_id } => {
                    if item.as_deref() == Some(item_id.as_str()) {
                        if let Err(error) = pacer.finish_item(&item_id) {
                            break 'say Err(error);
                        }
                        item_done = true;
                    }
                }
                K::Error { message, fatal, .. } => {
                    if fatal || item.is_none() {
                        break 'say Err(message);
                    }
                }
                K::Closed { reason } => {
                    if !item_done {
                        break 'say Err(reason);
                    }
                    events_over = true;
                }
                // Listening is off in this mode; nothing else applies.
                _ => {}
            }
        }
        // Pace the line into the SCO link, after any answer-settle hold.
        if egress_hold.is_none_or(|hold| now >= hold) {
            let chunk = pacer.take_ready(now, (sr as usize / 50).max(1));
            if !chunk.is_empty() {
                AudioLink::send_audio(bt, &chunk);
                sent_samples = sent_samples.saturating_add(chunk.len() as u64);
                first_sent.get_or_insert(now);
            }
        }
        let queued_ms = sent_samples.saturating_mul(1_000) / u64::from(sr);
        if item_done && first_sent.is_none() && pacer.active_item().is_none() {
            // The item ended without a sample: OAIY had nothing to say.
            break 'say Err("OAIY's line had no audio".into());
        }
        match first_sent {
            None if now.duration_since(started) > first_audio_within
                && egress_hold.is_none_or(|hold| now >= hold) =>
            {
                break 'say Err(format!(
                    "no audio from OAIY within {} ms",
                    first_audio_within.as_millis()
                ));
            }
            Some(first) => {
                let played_ms = now.duration_since(first).as_millis() as u64;
                // Said: the item is done, all of it went out, and it has had
                // the time to play.
                if item_done && pacer.active_item().is_none() && played_ms >= queued_ms {
                    break 'say Ok(());
                }
                if played_ms > queued_ms + 4_000 && pacer.active_item().is_some() {
                    break 'say Err("OAIY's line stopped arriving".into());
                }
            }
            None => {}
        }
        if now.duration_since(started) > OAIY_SAY_MAX {
            break 'say Err("the line took too long".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    said.dur = Duration::from_millis(sent_samples.saturating_mul(1_000) / u64::from(sr));
    if said.cancelled {
        bt.flush_tx_audio();
        session.stop("the call was ended");
    } else if let Err(error) = outcome {
        session.stop("the line could not be said");
        said.error = Some(error);
    } else {
        session.stop("said");
    }
    said
}

/// Say a fixed line (a screen message, a hold announcement, the failure
/// apology) in the route's voice: OAIY's first where `voice.oaiy` is set,
/// then Aokie's own TTS where `voice.own_tts` allows it, else nothing.
/// Returns the audio duration that reached the caller. `egress_hold` holds
/// the first audio until the answer has settled (screen messages).
#[cfg(all(aokie_radio, feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn say_fixed_line(
    bt: &mut dyn crate::backend::RadioBackend,
    synth: &crate::synth::SynthHandle,
    voice: LineVoice<'_>,
    call_id: &str,
    text: &str,
    sample_rate: u16,
    pace: &crate::speech_plan::PaceState,
    protected_max_ms: u32,
    mut ctl: Option<&mut ControlProbe<'_>>,
    egress_hold: Option<Instant>,
    first_audio_within: Duration,
) -> Duration {
    if let Some(config) = voice.oaiy {
        let said = say_in_oaiy_voice(
            bt,
            config,
            call_id,
            text,
            sample_rate,
            first_audio_within,
            ctl.as_deref_mut(),
            egress_hold,
        );
        if said.settled() {
            return said.dur;
        }
        eprintln!(
            "[aokie-plugin] OAIY's voice could not say a fixed line ({}){}",
            said.error.as_deref().unwrap_or("unknown"),
            if voice.own_tts {
                "; saying it with Aokie's own voice"
            } else {
                "; no other voice is available, so it is not said"
            }
        );
    }
    if !voice.own_tts {
        return Duration::ZERO;
    }
    speak_planned(
        bt,
        synth,
        text,
        sample_rate,
        None,
        None,
        ctl,
        pace,
        protected_max_ms,
        None,
        egress_hold,
    )
    .outcome
    .dur
}
