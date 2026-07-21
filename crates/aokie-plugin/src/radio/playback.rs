//! Paced SCO playout: `TtsChunkPlayback`, barge detection, ducking, cancel probes.

#[allow(unused_imports)]
use super::*;

// ── AOK-CTRL-001: cancellable, deadline-bounded call control ────────────────
//
// The pieces below are PURE (fake-clock testable — every decision takes `now`
// as a parameter, which is the clock seam the VOICE-001 deferral asked for):
// the reply-deadline watchdog, the call-level silence timer, the agent-hangup
// policy and the playout-drain math. The impure halves (the reply worker
// thread, the control probe inside TTS playback) live in `run_loop`.

/// An urgent operator action observed while speech was playing — detected by
/// [`ControlProbe`] inside the TTS chunk loop, EXECUTED by the caller right
/// after the speak returns (the `BluetoothManager` is mutably borrowed for
/// the whole playback, so the probe can only record the intent).
#[cfg(feature = "voice")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CancelAction {
    Hangup { op: Option<String> },
    Reject { op: Option<String> },
}

/// Control-channel probe threaded through speech playback so a hangup/reject
/// lands mid-SENTENCE (within ~one audio chunk, ≈20 ms) instead of waiting for
/// the sentence to finish playing. Urgent actions stop playback and are
/// recorded in `action`; every other control is parked, in arrival order, for
/// the main control loop (same contract as the old per-sentence poll).
#[cfg(feature = "voice")]
pub(super) struct ControlProbe<'a> {
    pub(super) rx: &'a std::sync::mpsc::Receiver<RadioControl>,
    pub(super) parked: &'a mut std::collections::VecDeque<RadioControl>,
    pub(super) action: Option<CancelAction>,
}

#[cfg(feature = "voice")]
impl<'a> ControlProbe<'a> {
    pub(super) fn new(
        rx: &'a std::sync::mpsc::Receiver<RadioControl>,
        parked: &'a mut std::collections::VecDeque<RadioControl>,
    ) -> Self {
        Self {
            rx,
            parked,
            action: None,
        }
    }

    /// Drain newly-arrived controls; `true` = an urgent action wants playback
    /// stopped NOW (sticky once set).
    pub(super) fn poll(&mut self) -> bool {
        if self.action.is_some() {
            return true;
        }
        while let Ok(c) = self.rx.try_recv() {
            match c {
                RadioControl::Hangup { op } => {
                    self.action = Some(CancelAction::Hangup { op });
                    return true;
                }
                RadioControl::Reject { op } => {
                    self.action = Some(CancelAction::Reject { op });
                    return true;
                }
                other => self.parked.push_back(other),
            }
        }
        false
    }
}

/// Result of speaking a phrase: how long the audio will play out, whether the
/// caller barged in (started speaking) mid-phrase so we cut it short, and —
/// when they did — the echo-cancelled audio of what they said WHILE Aokie was
/// still talking (audit AK-008). Barge detection needs sustained speech before
/// it trips, so without this capture the first words of an interruption (the
/// leading digits of a phone number, classically) were used for detection and
/// then thrown away — the STT only ever saw the part spoken after the trip.
/// Delivery-truth v1 (architecture guide §6.3): the numbers needed to
/// conservatively estimate how much of a CUT span the caller actually heard.
/// The engine knows exactly what was QUEUED to the SCO and how long audio had
/// been flowing when the cut landed; remote playout stays an estimate.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CutEstimate {
    /// Estimated audible playout at the cut: wall time since the first audio
    /// frame, capped by what was queued (audio can't play faster than it was
    /// fed).
    pub(super) audible_ms: u64,
    /// Everything pushed to the SCO TX queue (includes up to ~PLAYOUT_LEAD of
    /// audio that was flushed unplayed by the cut).
    pub(super) queued_ms: u64,
    /// The span's TOTAL synthesized duration — known exactly only when
    /// synthesis finished before the cut (queued + discarded pending PCM).
    pub(super) synthesized_ms: Option<u64>,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) struct SpeakOutcome {
    pub(super) dur: std::time::Duration,
    pub(super) barged: bool,
    /// AEC-cleaned caller speech captured during playback, at the SCO rate,
    /// starting a short pre-roll before their first above-threshold frame.
    /// Empty when nothing crossed the speech threshold (or half-duplex mode).
    pub(super) captured_speech: Vec<i16>,
    /// AOK-CTRL-001: playback was cut short by an urgent control (the probe's
    /// `action` says which) — the caller executes it right after this returns.
    pub(super) cancelled: bool,
    /// Phase 2 probe lane: the caller spoke a FLOOR COMMAND ("wait"/"stop")
    /// over this speech and the priority STT probe caught it mid-sentence —
    /// playback was cut (even through a protected span; explicit commands
    /// always win) and the dialogue should enter its pause state NOW.
    pub(super) commanded: Option<crate::duplex::CallerIntent>,
    /// §6.3 delivery truth: set when playback was CUT SHORT (barge / spoken
    /// command / urgent control) after some audio played — the numbers a
    /// consumer needs to estimate the audible prefix. `None` = the span
    /// played to its natural end, or nothing played at all.
    pub(super) cut_est: Option<CutEstimate>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub(super) struct HttpSpeechFallback {
    pub(super) endpoint: Option<String>,
    pub(super) failed_for_call: bool,
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
impl HttpSpeechFallback {
    pub(super) fn new(endpoint: Option<String>) -> Self {
        Self {
            endpoint: normalize_endpoint(endpoint),
            failed_for_call: false,
        }
    }

    pub(super) fn from_env(var: &str) -> Self {
        Self::new(std::env::var(var).ok())
    }

    pub(super) fn configure(&mut self, endpoint: Option<String>) {
        let endpoint = normalize_endpoint(endpoint);
        if endpoint != self.endpoint {
            self.endpoint = endpoint;
            self.failed_for_call = false;
        }
    }

    pub(super) fn reset_call(&mut self) {
        self.failed_for_call = false;
    }

    pub(super) fn endpoint_for_call(&self) -> Option<&str> {
        if self.failed_for_call {
            None
        } else {
            self.endpoint.as_deref()
        }
    }

    pub(super) fn mark_failed_for_call(&mut self) -> bool {
        if self.endpoint.is_some() && !self.failed_for_call {
            self.failed_for_call = true;
            true
        } else {
            false
        }
    }
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub(super) fn normalize_endpoint(endpoint: Option<String>) -> Option<String> {
    endpoint
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Feed one captured mic chunk through the echo canceller and update the
/// sustained-speech counter, returning `true` once the caller has spoken over
/// Aokie for `need` frames straight. `armed` gates the counting so the adaptive
/// filter has time to converge at a reply's onset (we still call
/// `process_capture` while un-armed â€” to keep the reference FIFO aligned with
/// capture â€” but never trip). Shared by the synth callback and the playout monitor.
///
/// AK-008: the cleaned audio is APPENDED to `captured` (not discarded) and the
/// first above-threshold frame's offset is recorded in `speech_start`, so the
/// caller's words spoken BEFORE the barge trips can be prepended to their
/// utterance instead of being lost to detection.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn detect_barge(
    aec: &mut crate::aec::EchoCanceller,
    mic: &[i16],
    frame: usize,
    capture_thr: f32,
    trip_thr: f32,
    armed: bool,
    speech_frames: &mut u32,
    need: u32,
    captured: &mut Vec<i16>,
    speech_start: &mut Option<usize>,
) -> bool {
    let cleaned = aec.process_capture(mic);
    scan_barge_frames(
        &cleaned,
        frame,
        capture_thr,
        trip_thr,
        armed,
        speech_frames,
        need,
        captured,
        speech_start,
    )
}

/// The scratchpad's own speech gate — the same level as the main VAD's
/// SPEECH_RMS. Live finding 2026-07-13: the echo canceller suppresses
/// near-end speech while the bot plays, so normal-volume overlap often sits
/// BETWEEN the VAD gate and the barge threshold — marking capture at the
/// barge threshold made the bot deaf to it. Capture marks at THIS gate; the
/// acoustic barge still requires the sustained, higher `trip_thr`.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) const CAPTURE_RMS: f32 = 350.0;

/// Pure scan half of [`detect_barge`] (unit-testable without an AEC): append
/// `cleaned` to the capture buffer, note the first frame above the CAPTURE
/// gate (armed or not — the scratchpad always hears), and report whether
/// sustained speech above the TRIP threshold barged (armed only).
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn scan_barge_frames(
    cleaned: &[i16],
    frame: usize,
    capture_thr: f32,
    trip_thr: f32,
    armed: bool,
    speech_frames: &mut u32,
    need: u32,
    captured: &mut Vec<i16>,
    speech_start: &mut Option<usize>,
) -> bool {
    let base = captured.len();
    captured.extend_from_slice(cleaned);
    let mut tripped = false;
    for (i, f) in cleaned.chunks(frame).enumerate() {
        let rms = crate::voice::frame_rms(f);
        if rms > capture_thr && speech_start.is_none() {
            *speech_start = Some(base + i * frame);
        }
        if !armed {
            continue;
        }
        if rms > trip_thr {
            *speech_frames += 1;
            if *speech_frames >= need {
                tripped = true;
                break;
            }
        } else {
            *speech_frames = speech_frames.saturating_sub(1);
        }
    }
    tripped
}

/// §12.3 synthetic-audio seam: the paced playback engine's view of the audio
/// transport. Live, this is the SCO link on the [`BluetoothManager`]; the
/// synthetic rig (mod `synthetic_audio`) drives a scripted link that serves
/// echo-mix mic frames and records everything that "played".
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) trait AudioLink {
    /// Non-blocking mic drain: one captured chunk, if any arrived.
    fn try_recv_audio(&mut self) -> Option<Vec<i16>>;
    /// Queue PCM for playout.
    fn send_audio(&mut self, pcm: &[i16]);
}

#[cfg(all(target_os = "windows", feature = "voice"))]
impl<'a> AudioLink for dyn crate::backend::RadioBackend + 'a {
    fn try_recv_audio(&mut self) -> Option<Vec<i16>> {
        crate::backend::RadioBackend::try_recv_audio(self).map(|audio| {
            // The paced TTS loop can own the SCO drain for seconds. Mirror
            // those caller frames into the bounded native media lane here so
            // monitoring/takeover never develops a TTS-sized audio hole.
            crate::remote_media::try_capture_sco_globally(&audio.samples, audio.sample_rate as u32);
            audio.samples
        })
    }
    fn send_audio(&mut self, pcm: &[i16]) {
        if !crate::remote_media::human_reserves_radio_globally() {
            crate::remote_media::try_capture_caller_output_globally(
                pcm,
                crate::backend::RadioBackend::get_sample_rate(self) as u32,
            );
            let _ = crate::backend::RadioBackend::send_audio(self, pcm);
        }
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) struct TtsChunkPlayback {
    pub(super) t0: std::time::Instant,
    pub(super) t_first: std::time::Instant,
    pub(super) first: bool,
    pub(super) samples: usize,
    pub(super) barged: bool,
    pub(super) frame: usize,
    pub(super) speech_frames: u32,
    pub(super) need: u32,
    pub(super) grace: std::time::Duration,
    /// AK-008: AEC-cleaned mic audio accumulated during playback (SCO rate).
    pub(super) captured: Vec<i16>,
    /// Offset in `captured` of the caller's first above-threshold frame.
    pub(super) speech_start: Option<usize>,
    pub(super) sample_rate: u16,
    /// AOK-CTRL-001: an urgent control stopped playback (see [`ControlProbe`]).
    pub(super) cancelled: bool,
    /// Interrupt policy for THIS span: `None` = yield the instant a barge
    /// trips (the classic behaviour); `Some(budget)` = a finish-the-span
    /// span (phone number, `[[important]]` detail) keeps playing for at
    /// most `budget` after the overlap started, then yields. Caller speech
    /// is captured throughout either way, and urgent controls (hangup /
    /// reject) still cut at chunk granularity.
    pub(super) finish_extra: Option<std::time::Duration>,
    /// When the barge first tripped (starts the finish budget).
    pub(super) barged_at: Option<std::time::Instant>,
    /// The probe lane caught a spoken floor command ("wait"/"stop") in the
    /// overlap capture: stop NOW — an explicit command beats every policy,
    /// protected spans included.
    pub(super) semantic: Option<crate::duplex::CallerIntent>,
    /// DUCKING (the "nudge" texture): caller speech was detected while THIS
    /// span was audibly playing — outbound gain ramps down to make room
    /// while the floor decision (finish clause / yield / barge) plays out.
    pub(super) speech_during_playback: bool,
    /// Current outbound gain (1.0 → DUCK_GAIN over ~100 ms once ducked).
    pub(super) duck_gain: f32,
    /// When the current duck began — after a short window the gain ramps
    /// BACK up, so a brief backchannel doesn't leave the rest of the
    /// sentence whispering. Sustained loud speech re-triggers it.
    pub(super) ducked_at: Option<std::time::Instant>,
}

/// Ducked outbound level once the caller talks over a playing span, and the
/// per-chunk (~20 ms) ramp step toward it.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) const DUCK_GAIN: f32 = 0.35;
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) const DUCK_RAMP_STEP: f32 = 0.15;

#[cfg(all(target_os = "windows", feature = "voice"))]
impl TtsChunkPlayback {
    pub(super) fn new(sample_rate: u16, finish_extra: Option<std::time::Duration>) -> Self {
        let t0 = std::time::Instant::now();
        Self {
            t0,
            t_first: t0,
            first: true,
            samples: 0,
            barged: false,
            frame: (sample_rate as usize / 100).max(80),
            speech_frames: 0,
            need: 22,
            grace: std::time::Duration::from_millis(350),
            captured: Vec::new(),
            speech_start: None,
            sample_rate,
            cancelled: false,
            finish_extra,
            barged_at: None,
            semantic: None,
            speech_during_playback: false,
            duck_gain: 1.0,
            ducked_at: None,
        }
    }

    /// Should playback stop NOW? Cancelled and spoken floor commands always
    /// stop (protected spans included); a barge stops a yield-policy span
    /// immediately and a finish-policy span once its bounded extension is
    /// spent.
    pub(super) fn stop_playback_now(&self, now: std::time::Instant) -> bool {
        if self.cancelled || self.semantic.is_some() {
            return true;
        }
        if !self.barged {
            return false;
        }
        match self.finish_extra {
            None => true,
            Some(budget) => self
                .barged_at
                .map(|at| now.duration_since(at) >= budget)
                .unwrap_or(false),
        }
    }

    /// Drain + echo-cancel whatever the mic delivered, watching for the
    /// caller talking over us (keeps the AEC reference FIFO aligned with
    /// capture and the scratchpad fed). Runs every paced-loop iteration —
    /// listening never stops, even while synthesis is still decoding.
    pub(super) fn poll_mic<L: AudioLink + ?Sized>(
        &mut self,
        link: &mut L,
        aec: &mut Option<&mut crate::aec::EchoCanceller>,
        barge_rms: Option<f32>,
        now: std::time::Instant,
    ) {
        if let (Some(a), Some(thr)) = (aec.as_deref_mut(), barge_rms) {
            // Arm only once audio is actually PLAYING (+ the AEC-convergence
            // grace from the first frame). The paced loop polls the mic while
            // synthesis is still warming up — first live test 2026-07-13: a
            // caller's "hello?" into that pre-audio silence tripped a false
            // barge and cancelled the greeting before its first sample, so
            // the call answered into dead air. Pre-audio speech is still
            // CAPTURED (scratchpad); it just never cancels what hasn't begun.
            let armed = !self.first && now.duration_since(self.t_first) >= self.grace;
            let had_speech = self.speech_start.is_some();
            while let Some(samples) = link.try_recv_audio() {
                if detect_barge(
                    a,
                    &samples,
                    self.frame,
                    CAPTURE_RMS,
                    thr,
                    armed,
                    &mut self.speech_frames,
                    self.need,
                    &mut self.captured,
                    &mut self.speech_start,
                ) && !self.barged
                {
                    self.barged = true;
                    self.barged_at = Some(now);
                }
            }
            // Speech that STARTED while this span was audibly playing ducks
            // the output (pre-audio speech never does — nothing to duck).
            if !had_speech && self.speech_start.is_some() && !self.first {
                self.speech_during_playback = true;
            }
            self.trim_idle_capture();
        }
    }

    /// Keep the capture bounded while the caller ISN'T speaking: with no
    /// speech detected yet, only a short pre-roll tail can ever matter, so
    /// trim to the last ~2 s. Once speech started, everything from its
    /// pre-roll onward is retained (bounded by the phrase length).
    pub(super) fn trim_idle_capture(&mut self) {
        if self.speech_start.is_some() {
            return;
        }
        let keep = (self.sample_rate as usize).saturating_mul(2).max(1);
        if self.captured.len() > keep * 2 {
            self.captured.drain(..self.captured.len() - keep);
        }
    }

    pub(super) fn push<L: AudioLink + ?Sized>(
        &mut self,
        link: &mut L,
        aec: &mut Option<&mut crate::aec::EchoCanceller>,
        barge_rms: Option<f32>,
        ctl: &mut Option<&mut ControlProbe<'_>>,
        pcm: &[i16],
        now: std::time::Instant,
    ) -> bool {
        // A gateway claim may arrive while this synchronous playback loop is
        // running. Its pending state reserves the radio immediately; stop at
        // this chunk boundary and let the main loop flush before physical ACK.
        if crate::remote_media::human_reserves_radio_globally() {
            self.cancelled = true;
            return false;
        }
        // AOK-CTRL-001: an urgent control (hangup/reject) cuts playback at
        // CHUNK granularity (~20 ms) — the old worst case was a whole sentence.
        if let Some(probe) = ctl.as_deref_mut() {
            if probe.poll() {
                self.cancelled = true;
                return false;
            }
        }
        if pcm.is_empty() {
            return !self.stop_playback_now(now);
        }
        if self.first {
            eprintln!(
                "[aokie-plugin] speaking (first audio in {:?})",
                self.t0.elapsed()
            );
            self.first = false;
            self.t_first = now;
        }
        // The nudge: the caller is talking over this span — duck the output
        // (ramped, click-free) so the bot audibly makes room while the floor
        // decision (finish the clause / yield / barge) plays out. The AEC
        // reference gets the SAME scaled samples that actually play.
        if (self.speech_during_playback && self.ducked_at.is_none()) || self.speech_frames >= 3 {
            self.ducked_at = Some(now);
        }
        let duck_target = match self.ducked_at {
            Some(at) if now.duration_since(at) < std::time::Duration::from_millis(1200) => {
                DUCK_GAIN
            }
            _ => 1.0,
        };
        if self.duck_gain > duck_target {
            self.duck_gain = (self.duck_gain - DUCK_RAMP_STEP).max(duck_target);
        } else if self.duck_gain < duck_target {
            self.duck_gain = (self.duck_gain + DUCK_RAMP_STEP).min(duck_target);
        }
        if self.duck_gain < 0.999 {
            let ducked: Vec<i16> = pcm
                .iter()
                .map(|&s| (s as f32 * self.duck_gain) as i16)
                .collect();
            if let Some(a) = aec.as_deref_mut() {
                a.feed_reference(&ducked);
            }
            link.send_audio(&ducked);
        } else {
            if let Some(a) = aec.as_deref_mut() {
                a.feed_reference(pcm);
            }
            link.send_audio(pcm);
        }
        self.samples += pcm.len();

        // Full-duplex: drain + echo-cancel the mic as we feed (keeps the
        // reference FIFO aligned with capture) and watch for the caller talking
        // over us. The batch is drained fully even after a trip — a finish-
        // policy span keeps playing (and keeps capturing) through its budget.
        self.poll_mic(link, aec, barge_rms, now);
        !self.stop_playback_now(now)
    }

    /// True once everything queued has PLAYED OUT (the paced loop owns the
    /// playout monitor since phase 2).
    pub(super) fn played_out(&self, now: std::time::Instant) -> bool {
        if self.first {
            return true; // nothing was ever queued
        }
        let playout = std::time::Duration::from_secs_f32(
            self.samples as f32 / self.sample_rate.max(1) as f32,
        );
        now.duration_since(self.t_first) >= playout
    }

    /// Consume the playback into its outcome (phase 2: no monitor loop here —
    /// the paced loop in `tts_speak` already ran playout to completion or cut).
    pub(super) fn into_outcome(mut self, text: &str, sample_rate: u16) -> SpeakOutcome {
        use std::time::Duration;
        // AK-008: hand back what the caller said while we were talking, from a
        // short pre-roll before their first above-threshold frame. The caller
        // (run_loop) prepends it to the STT buffer so the utterance is
        // complete — detection no longer eats the leading words.
        let captured_speech = match self.speech_start {
            Some(start) => {
                // ~450 ms pre-roll: the capture gate (CAPTURE_RMS) already
                // opens below the barge threshold, but a soft leading
                // consonant ("do you have…") can sit under even that for a
                // beat — keep a generous run-up so a barge never loses its
                // first word.
                let pre_roll = self.frame * 45;
                self.captured.split_off(start.saturating_sub(pre_roll))
            }
            None => Vec::new(),
        };

        eprintln!(
            "[aokie-plugin] spoke ({} chars -> {} samples @ {}Hz, span {:?}{}{}{})",
            text.chars().count(),
            self.samples,
            sample_rate,
            self.t0.elapsed(),
            if self.barged { ", BARGED-IN" } else { "" },
            if self.semantic.is_some() {
                ", SPOKEN COMMAND"
            } else {
                ""
            },
            if captured_speech.is_empty() {
                String::new()
            } else {
                format!(
                    ", captured {}ms of overlapped caller speech",
                    captured_speech.len() * 1000 / (sample_rate.max(1) as usize)
                )
            }
        );
        SpeakOutcome {
            dur: Duration::from_secs_f32(self.samples as f32 / sample_rate.max(1) as f32),
            // A spoken command IS the caller taking the floor.
            barged: self.barged || self.semantic.is_some(),
            captured_speech,
            cancelled: self.cancelled,
            commanded: self.semantic,
            // The paced loop (tts_speak) fills this in — only IT knows whether
            // the loop exited early and how much synthesized PCM it discarded.
            cut_est: None,
        }
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn http_tts_chunk_samples(sample_rate: u16) -> usize {
    (sample_rate as usize / 50).max(160)
}

/// AOK-VOICE-001: record the outcome of a live speech attempt in the shared
/// status — audible speech clears the TTS failure slot; a zero-audio outcome
/// (engine load / synthesis / endpoint failure) sets it, so health degrades
/// and auto-answer stops the moment the receptionist demonstrably can't speak.
/// A barged outcome with no audio is inconclusive (the caller cut it off) and
/// leaves the slot unchanged.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn note_tts_outcome(status: &RadioStatus, out: &SpeakOutcome) {
    let mut slot = status.tts_error.lock().unwrap();
    if out.dur > std::time::Duration::ZERO {
        *slot = None;
    } else if !out.barged && !out.cancelled {
        *slot = Some(
            "the last speech attempt produced no audio (TTS engine/endpoint failure) — check the voice models and the plugin log"
                .to_string(),
        );
    }
}

/// run_loop phase breadcrumbs (Phase-0 observability): which section the
/// loop last ENTERED. Coarse on purpose — the watchdog only needs to NAME the
/// stalled neighbourhood (the 2026-07-14 "silent call" forensics cost an hour
/// for lack of exactly this).
pub(super) mod loop_phase {
    pub const EVENTS: u8 = 1;
    pub const CALL_SETUP: u8 = 2;
    pub const MIC: u8 = 3;
    pub const RESULTS: u8 = 4;
    pub const TURN: u8 = 5;
    pub const WATCHDOGS: u8 = 6;
    pub const CONTROLS: u8 = 7;
    pub const TAIL: u8 = 8;
}

/// Marks the STT worker busy for the lifetime of one transcription job — the
/// loop watchdog reads it, so a wedged engine names itself in the log.
#[cfg(feature = "voice")]
pub(super) struct SttBusyGuard(pub(super) Arc<RadioStatus>);

#[cfg(feature = "voice")]
impl SttBusyGuard {
    pub(super) fn set(status: &Arc<RadioStatus>, samples: usize) -> Self {
        *status.stt_busy.lock().unwrap() = Some((std::time::Instant::now(), samples));
        Self(status.clone())
    }
}

#[cfg(feature = "voice")]
impl Drop for SttBusyGuard {
    fn drop(&mut self) {
        *self.0.stt_busy.lock().unwrap() = None;
    }
}
