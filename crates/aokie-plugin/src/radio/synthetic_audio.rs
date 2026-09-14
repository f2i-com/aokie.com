use super::*;
use std::time::Duration;

const SR: u16 = 8000; // CVSD-like; every gate in the engine is rate-relative
const CHUNK: usize = 160; // 20 ms @ 8 kHz — the paced loop's chunk size
const ECHO_DELAY: usize = CHUNK; // one-chunk round trip, well inside the AEC tail
const TRIP_RMS: f32 = 500.0; // the live default barge threshold

/// Scripted transport: the timeline stages one mic frame per step (echo
/// mix + caller script); everything the engine sends is recorded — it IS
/// the played audio, ducking included.
#[derive(Default)]
struct FakeLink {
    sent: Vec<i16>,
    mic: std::collections::VecDeque<Vec<i16>>,
}

impl AudioLink for FakeLink {
    fn try_recv_audio(&mut self) -> Option<Vec<i16>> {
        self.mic.pop_front()
    }
    fn send_audio(&mut self, pcm: &[i16]) {
        self.sent.extend_from_slice(pcm);
    }
}

/// A voice-like synthetic signal: two incommensurate tones, optionally
/// with a syllable-rate (4 Hz) amplitude envelope, scaled so the whole
/// buffer's RMS hits `rms`. Not a pure sine (the AEC preprocessor
/// special-cases those) and, with `am`, not stationary either (the
/// denoiser eats steady tones).
fn voice_signal(n: usize, phase0: usize, rms: f32, f1: f32, f2: f32, am: bool) -> Vec<i16> {
    let sr = SR as f32;
    let mut raw = Vec::with_capacity(n);
    let mut acc = 0.0f64;
    for i in 0..n {
        let t = (phase0 + i) as f32 / sr;
        let mut v = (2.0 * std::f32::consts::PI * f1 * t).sin()
            + 0.6 * (2.0 * std::f32::consts::PI * f2 * t).sin();
        if am {
            v *= 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
        }
        raw.push(v);
        acc += (v as f64) * (v as f64);
    }
    let cur = ((acc / n.max(1) as f64).sqrt()) as f32;
    let k = if cur > 0.0 { rms / cur } else { 0.0 };
    raw.iter()
        .map(|v| (v * k).clamp(-32000.0, 32000.0) as i16)
        .collect()
}

/// The phone-side echo of our playout: an attenuated copy of the sent
/// buffer, `ECHO_DELAY` samples behind real time. Returns the `n` mic
/// samples for the next step (leading zeros while history is short).
fn echo_chunk(sent: &[i16], n: usize, gain: f32) -> Vec<i16> {
    let end = sent.len().saturating_sub(ECHO_DELAY);
    let start = end.saturating_sub(n);
    let src = &sent[start..end];
    let mut out = vec![0i16; n];
    let off = n - src.len();
    for (i, &v) in src.iter().enumerate() {
        out[off + i] = (v as f32 * gain) as i16;
    }
    out
}

struct Timeline {
    /// How much bot audio the "synth" produces.
    bot_ms: u64,
    /// Steps before the first push — the paced loop polls the mic while
    /// synthesis warms up (greeting scenario).
    synth_warmup_ms: u64,
    /// Echo-path attenuation (0.0 = no echo returns).
    echo_gain: f32,
    /// Pre-converge the canceller on the echo path first (mid-call
    /// reality, and determinism — adaption transients stay out of the
    /// assertions).
    warm_aec: bool,
    /// Span interrupt policy (None = Yield).
    finish_extra: Option<Duration>,
    caller_onset_ms: Option<u64>,
    caller_dur_ms: u64,
    caller_rms: f32,
    caller_am: bool,
    /// Inject a probe-lane verdict ("stop") at this playout time.
    semantic_at_ms: Option<u64>,
    /// Inject an urgent control (hangup/reject) at this playout time.
    cancel_at_ms: Option<u64>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            bot_ms: 3000,
            synth_warmup_ms: 0,
            echo_gain: 0.3,
            warm_aec: true,
            finish_extra: None,
            caller_onset_ms: None,
            caller_dur_ms: 1000,
            caller_rms: 2500.0,
            caller_am: true,
            semantic_at_ms: None,
            cancel_at_ms: None,
        }
    }
}

struct RunResult {
    outcome: SpeakOutcome,
    stopped_at_ms: u64,
    sent: Vec<i16>,
}

/// Panic-safe guard: the rig drives the real engine WITHOUT a radio, so
/// this thread must not observe another (parallel) test's process-global
/// remote-media reservation — the 2026-07-19 full-suite flake.
struct RigIsolation;
impl RigIsolation {
    fn new() -> Self {
        crate::remote_media::set_test_rig_isolated(true);
        Self
    }
}
impl Drop for RigIsolation {
    fn drop(&mut self) {
        crate::remote_media::set_test_rig_isolated(false);
    }
}

/// Drive one bot utterance through the engine exactly like the paced
/// loop: 20 ms virtual steps, mic staged before each step from the echo
/// of what already played plus the caller script, `now` advanced on the
/// fake clock.
fn run(t: &Timeline) -> RunResult {
    let _isolation = RigIsolation::new();
    let mut aec_engine = crate::aec::EchoCanceller::new(SR as u32);
    let mut link = FakeLink::default();
    // The bot signal is generated PHASE-CONTINUOUS with the AEC warm-up
    // preamble (first rig run proved why, twice: a sent-history reset
    // reads as an echo-path change, and even a carrier-phase jump sprays
    // broadband energy through frequency bins the converged filter never
    // learned — either way the leak transient marks phantom caller
    // speech and cascades into ducking the whole run).
    let warm_len = if t.warm_aec && t.echo_gain > 0.0 {
        SR as usize * 2
    } else {
        0
    };
    let bot_len = (SR as u64 * t.bot_ms / 1000) as usize;
    let full = voice_signal(warm_len + bot_len, 0, 6000.0, 330.0, 730.0, true);
    if warm_len > 0 {
        // Teach the filter the echo path first (mid-call reality), feeding
        // reference + capture in the same order as the run loop, and keep
        // the warm-up audio as the link's sent-history.
        for chunk in full[..warm_len].chunks(CHUNK) {
            let mic = echo_chunk(&link.sent, CHUNK, t.echo_gain);
            aec_engine.feed_reference(chunk);
            let _ = aec_engine.process_capture(&mic);
            link.sent.extend_from_slice(chunk);
        }
    }
    let run_start = link.sent.len();
    let bot = &full[warm_len..];
    let mut playback = TtsChunkPlayback::new(SR, t.finish_extra);
    let base = playback.t0;
    let mut no_ctl: Option<&mut ControlProbe<'_>> = None;
    let warm_steps = t.synth_warmup_ms / 20;
    let mut caller_phase = 0usize;
    let mut pos = 0usize;
    let mut step: u64 = 0;
    loop {
        let now = base + Duration::from_millis(step * 20);
        let ms = step * 20;

        // Stage this step's mic frame: echo of what already played + the
        // caller script.
        let mut mic = if t.echo_gain > 0.0 {
            echo_chunk(&link.sent, CHUNK, t.echo_gain)
        } else {
            vec![0i16; CHUNK]
        };
        if let Some(on) = t.caller_onset_ms {
            if ms >= on && ms < on + t.caller_dur_ms {
                let c =
                    voice_signal(CHUNK, caller_phase, t.caller_rms, 210.0, 520.0, t.caller_am);
                caller_phase += CHUNK;
                for (m, v) in mic.iter_mut().zip(c) {
                    *m = m.saturating_add(v);
                }
            }
        }
        link.mic.push_back(mic);

        // Injected verdicts (probe lane / urgent control), by playout time.
        if let Some(at) = t.semantic_at_ms {
            if ms >= at && playback.semantic.is_none() {
                playback.semantic = Some(crate::duplex::CallerIntent::StopSpeaking);
            }
        }
        if let Some(at) = t.cancel_at_ms {
            if ms >= at {
                playback.cancelled = true;
            }
        }

        if step < warm_steps || pos >= bot.len() {
            // Synthesis warm-up / post-audio tail: the paced loop still
            // drains the mic every iteration.
            playback.poll_mic(&mut link, &mut Some(&mut aec_engine), Some(TRIP_RMS), now);
            if playback.stop_playback_now(now) {
                break;
            }
            if pos >= bot.len() && playback.played_out(now) {
                break;
            }
        } else {
            let n = CHUNK.min(bot.len() - pos);
            let ok = playback.push(
                &mut link,
                &mut Some(&mut aec_engine),
                Some(TRIP_RMS),
                &mut no_ctl,
                &bot[pos..pos + n],
                now,
            );
            pos += n;
            if !ok {
                break;
            }
        }
        step += 1;
        assert!(step < 4000, "synthetic timeline ran away");
    }
    let stopped_at_ms = step * 20;
    RunResult {
        outcome: playback.into_outcome("synthetic", SR),
        stopped_at_ms,
        sent: link.sent.split_off(run_start),
    }
}

fn sent_rms(sent: &[i16], from_ms: u64, to_ms: u64) -> f32 {
    let a = (from_ms as usize * SR as usize / 1000).min(sent.len());
    let b = (to_ms as usize * SR as usize / 1000).min(sent.len());
    crate::voice::frame_rms(&sent[a..b])
}

/// §12.3 "echo not being transcribed as caller speech": with the real
/// canceller converged on the echo path, three seconds of playout whose
/// echo returns at 30 % never marks caller speech, never barges, and
/// hands back no captured audio.
#[test]
fn echo_alone_is_cancelled_never_captured() {
    let r = run(&Timeline::default());
    assert!(!r.outcome.barged, "echo residual tripped the barge");
    assert!(!r.outcome.cancelled);
    assert!(
        r.outcome.captured_speech.is_empty(),
        "echo residual crossed the capture gate ({} samples captured)",
        r.outcome.captured_speech.len()
    );
    assert!(
        r.stopped_at_ms >= 3000,
        "playback must run to its natural end"
    );
}

/// §12.3 caller onset at 100 / 300 / 1000 ms into bot output: sustained
/// loud caller speech trips the acoustic barge shortly after onset —
/// never before the arming grace — and the overlap is captured for STT.
#[test]
fn caller_onset_trips_barge_at_each_offset() {
    for (onset, lo, hi) in [
        (100u64, 200u64, 500u64),
        (300, 400, 700),
        (1000, 1100, 1400),
    ] {
        let r = run(&Timeline {
            caller_onset_ms: Some(onset),
            caller_dur_ms: 2500,
            ..Timeline::default()
        });
        assert!(r.outcome.barged, "onset {onset} ms: barge never tripped");
        assert!(
            r.stopped_at_ms >= lo && r.stopped_at_ms <= hi,
            "onset {onset} ms: stopped at {} ms (expected {lo}..{hi})",
            r.stopped_at_ms
        );
        assert!(
            !r.outcome.captured_speech.is_empty(),
            "onset {onset} ms: overlap not captured"
        );
    }
}

#[test]
fn short_early_caller_interruption_yields_and_keeps_the_first_word() {
    let r = run(&Timeline {
        caller_onset_ms: Some(100), caller_dur_ms: 300,
        ..Timeline::default()
    });
    assert!(r.outcome.barged, "a short early interruption must take the floor");
    assert!(r.stopped_at_ms <= 400, "caller waited {} ms", r.stopped_at_ms);
    // This includes the quiet lead-in, not just the frame that tripped.
    assert!(r.outcome.captured_speech.len() >= SR as usize / 5);
}

#[test]
fn short_noise_does_not_take_the_floor() {
    for onset in [100, 600, 1500] {
        let r = run(&Timeline {
            caller_onset_ms: Some(onset), caller_dur_ms: 60,
            ..Timeline::default()
        });
        assert!(!r.outcome.barged, "noise burst at {onset} ms cut playback");
        assert!(r.stopped_at_ms >= 3000);
    }
}

/// §12.3 a ~200 ms interjection is REMEMBERED even without an acoustic
/// barge: quiet overlap (above the capture gate, below the trip
/// threshold) never stops playback but rides out in captured_speech.
#[test]
fn brief_quiet_interjection_captured_without_barge() {
    let r = run(&Timeline {
        caller_onset_ms: Some(600),
        caller_dur_ms: 200,
        caller_rms: 460.0,
        caller_am: false,
        ..Timeline::default()
    });
    assert!(!r.outcome.barged, "sub-trip speech must never barge");
    assert!(
        r.stopped_at_ms >= 3000,
        "playback must run to its natural end"
    );
    assert!(
        !r.outcome.captured_speech.is_empty(),
        "the scratchpad must hear the interjection"
    );
}

/// §12.3 "wait" inside the first 200 ms of a greeting: speech BEFORE any
/// audio has played is captured for the scratchpad but never cancels the
/// greeting (the paced loop polls the mic during synthesis warm-up; live
/// finding 2026-07-13 — a false barge here answered a call into dead air).
#[test]
fn pre_audio_speech_never_cancels_playback() {
    let r = run(&Timeline {
        synth_warmup_ms: 400,
        caller_onset_ms: Some(40),
        caller_dur_ms: 240,
        ..Timeline::default()
    });
    assert!(!r.outcome.barged, "pre-audio speech must not barge");
    assert!(
        r.stopped_at_ms >= 3400,
        "the greeting must play out fully (stopped at {} ms)",
        r.stopped_at_ms
    );
    assert!(
        !r.outcome.captured_speech.is_empty(),
        "but the scratchpad hears the pre-audio speech"
    );
}

/// §12.3 protected output with overlapping content: a FinishSpan span
/// (digits, [[important]]) rides through the barge for its bounded
/// extension, then yields; the same overlap stops a Yield span at once.
#[test]
fn finish_span_rides_bounded_extension_then_yields() {
    let overlap = Timeline {
        caller_onset_ms: Some(500),
        caller_dur_ms: 2500,
        ..Timeline::default()
    };
    let yielded = run(&overlap);
    let protected = run(&Timeline {
        finish_extra: Some(Duration::from_millis(700)),
        ..overlap
    });
    assert!(yielded.outcome.barged && protected.outcome.barged);
    assert!(
        protected.stopped_at_ms >= yielded.stopped_at_ms + 600,
        "the finish budget must audibly extend playback (yield {} ms vs finish {} ms)",
        yielded.stopped_at_ms,
        protected.stopped_at_ms
    );
    assert!(
        protected.stopped_at_ms <= yielded.stopped_at_ms + 1000,
        "but the budget is bounded (yield {} ms vs finish {} ms)",
        yielded.stopped_at_ms,
        protected.stopped_at_ms
    );
}

/// §12.3 explicit stop during protected output: a spoken floor command
/// (probe-lane verdict) cuts even a protected span immediately — explicit
/// commands always beat policy.
#[test]
fn spoken_stop_cuts_protected_span() {
    let r = run(&Timeline {
        finish_extra: Some(Duration::from_millis(5000)),
        caller_onset_ms: Some(500),
        caller_dur_ms: 2500,
        semantic_at_ms: Some(900),
        ..Timeline::default()
    });
    assert!(
        r.stopped_at_ms <= 1000,
        "spoken command must cut instantly (stopped at {} ms with a 5 s budget)",
        r.stopped_at_ms
    );
    assert!(matches!(
        r.outcome.commanded,
        Some(crate::duplex::CallerIntent::StopSpeaking)
    ));
    assert!(
        r.outcome.barged,
        "a spoken command IS the caller taking the floor"
    );
}

/// §12.3 call end during queued playout: an urgent control (hangup /
/// reject) cancels mid-span regardless of policy.
#[test]
fn urgent_control_cancels_mid_playout() {
    let r = run(&Timeline {
        finish_extra: Some(Duration::from_millis(5000)),
        cancel_at_ms: Some(700),
        ..Timeline::default()
    });
    assert!(r.outcome.cancelled);
    assert!(
        r.stopped_at_ms <= 800,
        "urgent control must cut at chunk granularity (stopped at {} ms)",
        r.stopped_at_ms
    );
}

/// §2.2 the nudge: caller overlap DUCKS the playing span (~35 %) and the
/// gain ramps back up once they stop — a brief interjection doesn't leave
/// the rest of the sentence whispering. Measured on the actually-sent
/// samples, ducking applied.
#[test]
fn overlap_ducks_output_then_ramps_back() {
    let r = run(&Timeline {
        bot_ms: 4000,
        // A large finish budget holds the floor so ducking is observable
        // in isolation from the yield decision.
        finish_extra: Some(Duration::from_millis(10_000)),
        caller_onset_ms: Some(800),
        caller_dur_ms: 400,
        ..Timeline::default()
    });
    let full = sent_rms(&r.sent, 200, 700);
    let ducked = sent_rms(&r.sent, 950, 1200);
    let recovered = sent_rms(&r.sent, 3000, 3800);
    assert!(
        ducked < full * 0.6,
        "output must duck under overlap (ducked {ducked} vs full {full})"
    );
    assert!(
        recovered > full * 0.8,
        "gain must ramp back after the overlap ends (recovered {recovered} vs full {full})"
    );
}

/// The probe lane end-to-end minus audio: a result that parses to a floor
/// command parks as the pending verdict; content partials feed
/// sentence-boundary steering; one probe in flight at a time; stale
/// generations are dropped.
#[test]
fn probe_lane_commands_content_and_backpressure() {
    let (stt_tx, stt_rx) = std::sync::mpsc::channel();
    let (res_tx, res_rx) = std::sync::mpsc::channel();
    let status = RadioStatus::default();
    let mut lane = SttProbeLane::new(&stt_tx, &res_rx, 7, &status);

    // A playback with ≥400 ms of marked speech ships exactly ONE probe.
    let mut p = TtsChunkPlayback::new(SR, None);
    p.captured = vec![100i16; SR as usize];
    p.speech_start = Some(0);
    lane.maybe_probe(&p);
    assert!(matches!(
        stt_rx.try_recv(),
        Ok(SttWork::Probe { generation: 7, .. })
    ));
    lane.maybe_probe(&p);
    assert!(stt_rx.try_recv().is_err(), "at most one probe in flight");

    // Stale-generation results are dropped; a matching "wait" parks.
    res_tx
        .send(SttResult {
            generation: 6,
            utterance: lane.lane_id,
            text: "stop".into(),
        })
        .unwrap();
    assert!(lane.check().is_none(), "stale generation must be dropped");
    // A FOREIGN-LANE result (a previous reply's in-flight probe landing
    // late — the 066d2237 class) is dropped even with the right
    // generation: it must never park a command or feed content.
    res_tx
        .send(SttResult {
            generation: 7,
            utterance: lane.lane_id + 1,
            text: "say that again".into(),
        })
        .unwrap();
    assert!(lane.check().is_none(), "foreign lane id must be dropped");
    res_tx
        .send(SttResult {
            generation: 7,
            utterance: lane.lane_id,
            text: "wait".into(),
        })
        .unwrap();
    assert!(matches!(
        lane.check(),
        Some(crate::duplex::CallerIntent::Pause)
    ));

    // Substantive content steers at sentence boundaries; backchannels don't.
    res_tx
        .send(SttResult {
            generation: 7,
            utterance: lane.lane_id,
            text: "yeah".into(),
        })
        .unwrap();
    assert!(lane
        .substantive_content("the weather is lovely today")
        .is_none());
    res_tx
        .send(SttResult {
            generation: 7,
            utterance: lane.lane_id,
            text: "actually I need to change my order".into(),
        })
        .unwrap();
    assert_eq!(
        lane.substantive_content("the weather is lovely today")
            .as_deref(),
        Some("actually I need to change my order")
    );

    // Clause-level replanning (§7): the same substantive content trips
    // the MID-SPAN yield against the lane's bot context — once. A fresh
    // lane with only a backchannel on the pad never trips it.
    lane.set_bot_context("the weather is lovely today".to_string());
    assert!(
        lane.substantive_overlap(),
        "substantive comment must take the floor"
    );
    assert!(!lane.substantive_overlap(), "fires at most once per lane");

    let (_stt_tx2, _r) = std::sync::mpsc::channel::<SttWork>();
    let (res_tx2, res_rx2) = std::sync::mpsc::channel();
    let mut lane2 = SttProbeLane::new(&_stt_tx2, &res_rx2, 7, &status);
    lane2.set_bot_context("your booking is confirmed".to_string());
    res_tx2
        .send(SttResult {
            generation: 7,
            utterance: lane2.lane_id,
            text: "yeah".into(),
        })
        .unwrap();
    assert!(
        !lane2.substantive_overlap(),
        "a backchannel never steals the floor"
    );
}
