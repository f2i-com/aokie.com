//! Span-planned speech: `tts_speak`, the STT probe lane and audible-prefix estimation.

#[allow(unused_imports)]
use super::*;

/// §6.3 delivery-truth v1 (alignment fallback #4 — conservative duration-
/// weighted estimation): which PREFIX of a cut span's display text did the
/// caller plausibly hear? The local engines expose no word/phoneme alignment,
/// so the estimate is deliberately conservative — UNDERCLAIM, never overclaim:
/// duration-weighted against the span's exact synthesized total when synthesis
/// finished before the cut, intersected with a chars-per-second ceiling
/// (~14 cps at rate 1.0), then FLOORED to a word boundary (never half a word).
/// The fraction maps display chars, not TTS-normalized chars — digit expansion
/// skews seconds-per-display-char, which the floor + min() absorb for v1.
/// Returns the prefix and whether the boundary is uncertain (true for
/// anything short of a full play). Underclaiming costs only a little natural
/// redundancy in the repair; overclaiming loses information the caller never
/// heard.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn estimate_audible_prefix(text: &str, rate: f32, cut: &CutEstimate) -> (String, bool) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || cut.audible_ms == 0 {
        return (String::new(), true);
    }
    if let Some(total) = cut.synthesized_ms {
        if cut.audible_ms >= total {
            // The cut landed after everything queued had played out — the
            // whole span was plausibly heard.
            return (text.to_string(), false);
        }
    }
    let cps = 14.0f32 * rate.max(0.1);
    let by_cps = (cut.audible_ms as f32 / 1000.0 * cps) as usize;
    let est = match cut.synthesized_ms {
        Some(total) if total > 0 => {
            let frac = (cut.audible_ms as f32 / total as f32).min(1.0);
            ((frac * chars.len() as f32) as usize).min(by_cps)
        }
        _ => by_cps,
    };
    if est >= chars.len() {
        return (text.to_string(), true);
    }
    // Floor to the previous word boundary — never claim half a word.
    let prefix: String = chars[..est].iter().collect();
    let cut_at = prefix.rfind(char::is_whitespace).unwrap_or(0);
    (prefix[..cut_at].trim_end().to_string(), true)
}

/// How far ahead of real playout the SCO TX queue is kept topped up (phase 2).
/// Small enough that a cancel/flush silences the line almost immediately;
/// large enough to ride out pump-iteration jitter without underruns.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) const PLAYOUT_LEAD: std::time::Duration = std::time::Duration::from_millis(200);

/// Pure pacing decision: may another chunk be queued yet? The first chunk
/// always may (it starts the playout clock); after that the queued total may
/// lead the playout clock by at most [`PLAYOUT_LEAD`].
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn may_queue_more(
    first: bool,
    queued_samples: usize,
    sample_rate: u16,
    elapsed_since_first: std::time::Duration,
    lead: std::time::Duration,
) -> bool {
    if first {
        return true;
    }
    let queued =
        std::time::Duration::from_secs_f32(queued_samples as f32 / sample_rate.max(1) as f32);
    queued < elapsed_since_first + lead
}

/// The priority STT control lane (phase 2 + plan §3.2): while Aokie speaks,
/// SNAPSHOT the caller's overlap capture every ~500 ms and have the STT
/// worker transcribe it on the dedicated probe channel. A probe that parses
/// to a hard floor command ("wait"/"stop") cuts playback MID-SENTENCE — even
/// through a protected span. Probes never touch the content buffer; the full
/// utterance still arrives at its normal endpoint.
/// Monotonic probe-lane id source (066d2237 stale-probe fencing). Lane 0 is
/// reserved for the main loop's live-hypothesis lane; playback lanes start
/// at 1. Each lane only ever consumes results carrying ITS id, so a probe of
/// the caller's previous (already-answered) utterance can never be credited
/// as live overlap by the NEXT reply's lane.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) static PROBE_LANE_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// The live-hypothesis lane's fixed probe-lane id (main loop, bot silent).
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) const LIVE_HYP_LANE: u32 = 0;

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) struct SttProbeLane<'a> {
    pub(super) stt_tx: &'a std::sync::mpsc::Sender<SttWork>,
    pub(super) results: &'a std::sync::mpsc::Receiver<SttResult>,
    pub(super) status: &'a RadioStatus,
    pub(super) generation: u64,
    /// This lane's unique probe id — results with a foreign id are dropped.
    pub(super) lane_id: u32,
    pub(super) last_probe_at: Option<std::time::Instant>,
    pub(super) probed_len: usize,
    pub(super) in_flight: u32,
    /// The live scratchpad TEXT: the latest content partial heard over the
    /// bot's speech. Read at sentence boundaries so the reply can yield to
    /// substantive caller speech instead of finishing a stale paragraph.
    pub(super) content: String,
    pub(super) pending_command: Option<crate::duplex::CallerIntent>,
    /// What the bot has said/is saying — the echo comparator for the
    /// mid-span substantive check (the caller sets it before each span).
    pub(super) bot_context: String,
    /// The mid-span yield fires at most once per lane: after it, the span
    /// is already yielding and the reply path owns the floor decision.
    pub(super) mid_span_fired: bool,
    /// Some span of THIS reply has audibly played: ongoing overlap speech in
    /// later spans/gaps may yield even though it did not START inside the
    /// current span (sentences are separate spans - the per-span
    /// speech_during_playback flag alone went deaf across boundaries; user
    /// report 2026-07-14: 'it doesn't seem to be hearing me while talking').
    pub(super) audio_played: bool,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
impl<'a> SttProbeLane<'a> {
    pub(super) fn new(
        stt_tx: &'a std::sync::mpsc::Sender<SttWork>,
        results: &'a std::sync::mpsc::Receiver<SttResult>,
        generation: u64,
        status: &'a RadioStatus,
    ) -> Self {
        // Discard results from a PREVIOUS span: a "wait" heard over sentence
        // 3 must not cut sentence 4 seconds later — probes are instant-or-
        // nothing; the turn-level grammar still catches the command. The
        // unique lane id below covers the residual race this drain cannot: a
        // probe still IN FLIGHT here lands after construction and would
        // otherwise be credited to this lane (066d2237).
        while results.try_recv().is_ok() {}
        Self {
            stt_tx,
            results,
            status,
            generation,
            lane_id: PROBE_LANE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            last_probe_at: None,
            probed_len: 0,
            in_flight: 0,
            content: String::new(),
            pending_command: None,
            bot_context: String::new(),
            mid_span_fired: false,
            audio_played: false,
        }
    }

    /// The pump marks the reply audible after each span that produced audio.
    pub(super) fn note_audio_played(&mut self) {
        self.audio_played = true;
    }

    /// PROMOTED floor path: perform the once-per-reply yield bookkeeping the
    /// legacy substantive_overlap() did (latch + midSpanYields + log).
    pub(super) fn floor_yield_once(&mut self) -> bool {
        if self.mid_span_fired {
            return false;
        }
        self.mid_span_fired = true;
        self.status.mid_span_yields.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "[aokie-plugin] floor yield — substantive overlap takes the clause: {}",
            content_for_log(self.content.trim())
        );
        true
    }

    /// The bot text spoken so far (+ the sentence about to play): the echo
    /// comparator for [`Self::substantive_overlap`].
    pub(super) fn set_bot_context(&mut self, ctx: String) {
        self.bot_context = ctx;
    }

    /// Clause-level replanning (guide §7.2): SUBSTANTIVE overlap content
    /// (≥3 words, not a backchannel, not the bot's own echo) takes the floor
    /// like an acoustic barge — the CURRENT span yields under its own
    /// interrupt policy instead of playing on to its sentence boundary,
    /// which is what made a mid-sentence comment feel ignored for seconds.
    /// Fires at most once per lane; counted as `midSpanYields`.
    pub(super) fn substantive_overlap(&mut self) -> bool {
        if self.mid_span_fired {
            return false;
        }
        let ctx = std::mem::take(&mut self.bot_context);
        let hit = self.substantive_content(&ctx).is_some();
        self.bot_context = ctx;
        if hit {
            self.mid_span_fired = true;
            self.status.mid_span_yields.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[aokie-plugin] substantive overlap mid-span — yielding the clause to {}",
                content_for_log(self.content.trim())
            );
        }
        hit
    }

    /// Non-consuming snapshot for the FloorManager SHADOW (guide §7.1/P1-8):
    /// the latest overlap transcript + whether it passes the substantive gate.
    /// Never fires the yield, never takes the command — pure evidence.
    pub(super) fn shadow_snapshot(&mut self) -> (String, bool) {
        let ctx = std::mem::take(&mut self.bot_context);
        let substantive = self.substantive_content(&ctx).is_some();
        self.bot_context = ctx;
        (self.content.clone(), substantive)
    }

    /// Ship a probe when speech has been running long enough and enough NEW
    /// audio arrived since the last one.
    pub(super) fn maybe_probe(&mut self, playback: &TtsChunkPlayback) {
        let Some(start) = playback.speech_start else {
            return;
        };
        let sr = playback.sample_rate.max(1) as usize;
        let since_speech = playback.captured.len().saturating_sub(start);
        if since_speech < (sr * 3) / 10 {
            return; // <300 ms of speech so far
        }
        // 450 ms cadence (was 700): the mid-span substantive yield is only as
        // fast as the probes feeding it. Still ≤1 in flight — the serial STT
        // worker regression stays fixed; a shorter gate just re-arms sooner.
        if let Some(at) = self.last_probe_at {
            if at.elapsed() < std::time::Duration::from_millis(450) {
                return;
            }
        }
        // At most ONE probe in flight (live regression 2026-07-13: probes
        // queueing ahead of the caller's REAL utterance on the serial STT
        // worker delayed every reply). A lost result unblocks after 1.5 s
        // via the last_probe_at gate above falling through here.
        if self.in_flight > 0 {
            match self.last_probe_at {
                Some(at) if at.elapsed() >= std::time::Duration::from_millis(1500) => {
                    self.in_flight = 0;
                }
                _ => return,
            }
        }
        if playback.captured.len() <= self.probed_len {
            return; // nothing new since the last probe
        }
        // Last ~1.2 s window, from a little before speech onset.
        let window = sr + sr / 5;
        let from = start
            .saturating_sub(playback.frame * 30)
            .max(playback.captured.len().saturating_sub(window));
        let snapshot =
            crate::voice::to_f32_16k(&playback.captured[from..], playback.sample_rate as u32);
        self.last_probe_at = Some(std::time::Instant::now());
        self.probed_len = playback.captured.len();
        self.in_flight += 1;
        self.status.probes_sent.fetch_add(1, Ordering::Relaxed);
        let _ = self.stt_tx.send(SttWork::Probe {
            generation: self.generation,
            lane: self.lane_id,
            samples: snapshot,
        });
    }

    /// Fold newly-arrived probe transcripts into the lane: hard floor
    /// commands park in `pending_command`; everything else becomes the
    /// current scratchpad content (latest, longest partial wins).
    pub(super) fn drain(&mut self) {
        while let Ok(res) = self.results.try_recv() {
            // A foreign lane's result (a probe sent by a PREVIOUS reply's
            // lane, still in flight when this lane was built) is not ours:
            // it neither clears our in-flight slot nor feeds our content —
            // crediting it cut two live replies on call 066d2237.
            if res.utterance != self.lane_id {
                continue;
            }
            self.in_flight = self.in_flight.saturating_sub(1);
            if res.generation != self.generation {
                continue;
            }
            let intent = crate::duplex::parse_caller_intent(&res.text);
            if matches!(
                intent,
                crate::duplex::CallerIntent::Pause | crate::duplex::CallerIntent::StopSpeaking
            ) {
                eprintln!(
                    "[aokie-plugin] probe caught a spoken floor command ({intent:?}): {}",
                    content_for_log(&res.text)
                );
                self.status.probe_commands.fetch_add(1, Ordering::Relaxed);
                self.pending_command = Some(intent);
            } else if res.text.trim().len() > self.content.trim().len() {
                self.content = res.text;
            }
        }
    }

    /// A probe transcript that parses to a hard floor command.
    pub(super) fn check(&mut self) -> Option<crate::duplex::CallerIntent> {
        self.drain();
        self.pending_command.take()
    }

    /// Sentence-boundary steering: SUBSTANTIVE caller speech on the
    /// scratchpad (≥3 words, not a backchannel, not the bot's own echo)
    /// means the reply should yield here and respond to it.
    pub(super) fn substantive_content(&mut self, bot_text_so_far: &str) -> Option<String> {
        self.drain();
        let text = self.content.trim();
        if text.is_empty() {
            return None;
        }
        let words = text.split_whitespace().count();
        if words < 3 || crate::duplex::is_backchannel(text) {
            return None;
        }
        if looks_like_echo(text, bot_text_so_far) {
            return None;
        }
        Some(text.to_string())
    }
}

/// Voice build only: speak `text` through the phase-2 paced engine. Synthesis
/// runs on the synth WORKER (crate::synth); this loop — on the radio thread —
/// paces the produced PCM into the SCO TX queue no more than
/// [`PLAYOUT_LEAD`] ahead of real playout while continuously draining the mic
/// (AEC + barge + scratchpad capture) and, when a probe lane is provided,
/// shipping overlap snapshots to the STT worker so a spoken "wait"/"stop"
/// cuts playback mid-sentence. With `aec`/`barge_rms` `None` it's the plain
/// half-duplex path (caller relies on the mute), still gaining the ~200 ms
/// cancel latency. No-op with no SCO channel (sample_rate 0) or empty text.
///
/// `rate` is the span's speaking-speed multiplier (1.0 = normal; WSOLA on the
/// worker). `finish_extra` is the span's interrupt policy.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn tts_speak(
    bt: &mut dyn crate::backend::RadioBackend,
    synth: &crate::synth::SynthHandle,
    text: &str,
    sample_rate: u16,
    mut aec: Option<&mut crate::aec::EchoCanceller>,
    barge_rms: Option<f32>,
    mut ctl: Option<&mut ControlProbe<'_>>,
    rate: f32,
    finish_extra: Option<std::time::Duration>,
    mut probe: Option<&mut SttProbeLane<'_>>,
    // Post-answer settle (greeting only): hold SCO EGRESS until this instant.
    // Synthesis, mic capture, the probe lane and urgent controls all keep
    // running — the caller's early words become overlap capture that seeds
    // their first turn AFTER the speech, instead of a normal turn whose
    // agent reply cancels the greeting (live call e77457c6 2026-07-18: an
    // arm-level settle hold left the line "listening", the caller's pickup
    // "Yeah." got a full reply, and the already-conversed guard skipped the
    // personalized greeting entirely).
    egress_hold: Option<std::time::Instant>,
) -> SpeakOutcome {
    use std::time::Duration;
    let none = SpeakOutcome {
        dur: Duration::ZERO,
        barged: false,
        captured_speech: Vec::new(),
        cancelled: false,
        commanded: None,
        cut_est: None,
    };
    if sample_rate == 0 || text.trim().is_empty() {
        return none;
    }
    // Speech-normalize ONCE at the chokepoint (greeting, agent sentences and
    // operatorSpeak all funnel through here): "10 a.m.," → "10 AM," — dotted
    // abbreviations against punctuation make the TTS stutter audibly.
    let text = &crate::speech_wire::normalize_speech_text(text);
    // Voice from AOKIE_TTS_VOICE, shared by HTTP and in-process synthesis.
    let voice = std::env::var("AOKIE_TTS_VOICE").unwrap_or_default();
    let epoch = synth.begin(text, &voice, rate, sample_rate as u32);

    let mut playback = TtsChunkPlayback::new(sample_rate, finish_extra);
    let mut pending: std::collections::VecDeque<i16> = std::collections::VecDeque::new();
    let mut synth_done = false;
    let mut synth_err: Option<String> = None;
    let chunk = http_tts_chunk_samples(sample_rate); // ~20 ms
    let mut stopped = false;
    // §6.3 delivery truth: did playback run to completion, or was it cut?
    // Only the CUT case needs an audible-prefix estimate.
    let mut natural_end = false;
    // FloorManager SHADOW (guide §7.1/P1-8): the fused decision the shadow
    // would make, tracked across the span. Logged on transitions; compared
    // with the actual outcome at span end. Behavior-neutral by construction.
    let mut shadow_prev: Option<crate::duplex::FloorDecision> = None;
    let mut shadow_worst: u8 = 0;
    // Wall-clock safety floor: this loop runs ON the radio thread, so while
    // it spins nothing else — event processing, control commands, a
    // device-side hangup — is serviced. The natural-end test waits for the
    // SCO TX queue to DRAIN at real time; if the audio path is wedged (a
    // call-hold transition left this leg inactive, the dongle stalled) the
    // queue never drains and the loop would spin forever, hanging the whole
    // radio (observed live 2026-07-15: an auto-hold double-swap left the
    // resumed leg's SCO not draining and the call could not be hung up).
    // These two guards guarantee tts_speak ALWAYS returns, so the radio
    // thread always gets back to servicing events + controls.
    let loop_start = std::time::Instant::now();
    loop {
        let now = std::time::Instant::now();
        // Absolute backstop: no single utterance legitimately pumps this
        // long. If nothing was ever queued, synthesis is dead; either way,
        // stop so the radio thread can service events (hangup) again.
        let elapsed = now.duration_since(loop_start);
        if playback.first {
            if elapsed > Duration::from_secs(8) {
                eprintln!(
                    "[aokie-plugin] TTS produced no audio in 8s — abandoning playout to keep the radio responsive"
                );
                break;
            }
        } else {
            // Something was queued: it must drain at ~real time. If far more
            // wall-clock than the queued audio's own duration has elapsed and
            // it still is not played out, the SCO sink is wedged.
            let queued_ms = playback.samples as u64 * 1000 / u64::from(sample_rate.max(1));
            let played_ms = now.duration_since(playback.t_first).as_millis() as u64;
            if played_ms > queued_ms + 4000 {
                eprintln!(
                    "[aokie-plugin] TTS playout wedged ({played_ms}ms elapsed vs {queued_ms}ms of audio queued) — SCO not draining, abandoning to keep the radio responsive"
                );
                break;
            }
        }
        // Urgent controls cut even while we're idling between frames.
        if let Some(p) = ctl.as_deref_mut() {
            if p.poll() {
                playback.cancelled = true;
            }
        }
        if playback.cancelled {
            break;
        }
        // Collect whatever the worker produced (non-blocking).
        loop {
            match synth.out_rx.try_recv() {
                Ok(crate::synth::SynthOut::Frames { epoch: e, pcm }) if e == epoch => {
                    pending.extend(pcm);
                }
                Ok(crate::synth::SynthOut::Done { epoch: e }) if e == epoch => {
                    synth_done = true;
                }
                Ok(crate::synth::SynthOut::Failed { epoch: e, error }) if e == epoch => {
                    synth_done = true;
                    synth_err = Some(error);
                }
                Ok(_) => {} // stale epoch — discard
                Err(_) => break,
            }
        }
        // Top up the SCO TX queue, bounded to the playout lead. The egress
        // hold gates ONLY this send path — everything else in the loop
        // (synthesis collection, mic drain, probe lane, controls) runs on.
        while !pending.is_empty()
            && egress_hold.is_none_or(|g| now >= g)
            && may_queue_more(
                playback.first,
                playback.samples,
                sample_rate,
                if playback.first {
                    Duration::ZERO
                } else {
                    now.duration_since(playback.t_first)
                },
                PLAYOUT_LEAD,
            )
        {
            let n = chunk.min(pending.len());
            let frame: Vec<i16> = pending.drain(..n).collect();
            if !playback.push(bt, &mut aec, barge_rms, &mut ctl, &frame, now) {
                stopped = true;
                break;
            }
        }
        if stopped {
            break;
        }
        // Listening never stops: drain the mic even with nothing to queue.
        playback.poll_mic(bt, &mut aec, barge_rms, now);
        // Priority control lane: snapshot overlap speech + act on a spoken
        // floor command mid-sentence (beats protected spans by design).
        if let Some(lane) = probe.as_deref_mut() {
            lane.maybe_probe(&playback);
            if let Some(intent) = lane.check() {
                playback.semantic = Some(intent);
            }
            // Fused floor decision (guide §7.1). PROMOTED: it owns the
            // substantive-yield and duck actions; the spoken-command lane
            // above and the energy hard-barge in poll_mic stay authoritative
            // for their cases. Shadow-only mode keeps the legacy chain.
            let (txt, substantive) = lane.shadow_snapshot();
            let ev = crate::duplex::FloorEvidence {
                bot_audible: !playback.first,
                speech_frames: playback.speech_frames,
                // speech_start is a sample OFFSET into the capture buffer —
                // overlap duration = captured samples since it, at the mic
                // rate.
                overlap_ms: playback
                    .speech_start
                    .map(|s| {
                        (playback.captured.len().saturating_sub(s) as u64 * 1000
                            / playback.sample_rate.max(1) as u64) as u32
                    })
                    .unwrap_or(0),
                stable_text: txt,
                substantive,
                protected_span: finish_extra.is_some(),
                barge_energy: playback.barged,
                speech_began_in_reply: playback.speech_during_playback
                    || (lane.audio_played && playback.speech_start.is_some()),
            };
            let (dec, reason) = crate::duplex::shadow_floor_decision(&ev);
            shadow_worst = shadow_worst.max(crate::duplex::floor_decision_rank(dec));
            if shadow_prev != Some(dec) {
                shadow_prev = Some(dec);
                match dec {
                    crate::duplex::FloorDecision::CutNow
                    | crate::duplex::FloorDecision::PauseAndRetain => {
                        lane.status
                            .floor_shadow_cuts
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    crate::duplex::FloorDecision::YieldAtBoundary => {
                        lane.status
                            .floor_shadow_yields
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    crate::duplex::FloorDecision::Duck => {
                        lane.status
                            .floor_shadow_ducks
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                if dec != crate::duplex::FloorDecision::Continue {
                    if floor_manager_owns() {
                        eprintln!("[aokie-plugin] floor: {dec:?} ({reason})");
                    } else {
                        eprintln!("[aokie-plugin] floor shadow: {dec:?} ({reason})");
                    }
                }
            }
            if floor_manager_owns() {
                match dec {
                    crate::duplex::FloorDecision::CutNow
                    | crate::duplex::FloorDecision::YieldAtBoundary
                        if matches!(
                            reason,
                            "substantive_overlap" | "substantive_over_protected"
                        ) =>
                    {
                        // Policy-aware soft barge: a protected/digit span
                        // still finishes its bounded extension; the cut tail
                        // rides the nudge into the next reply.
                        if !playback.barged && lane.floor_yield_once() {
                            playback.barged = true;
                            playback.barged_at = Some(now);
                        }
                    }
                    crate::duplex::FloorDecision::Duck => {
                        if playback.ducked_at.is_none() {
                            playback.ducked_at = Some(now);
                        }
                    }
                    // explicit_stop/explicit_wait belong to the spoken-command
                    // lane (lane.check() above); energy_barge already acted in
                    // poll_mic; Continue/StaySilent need nothing.
                    _ => {}
                }
            } else if !playback.barged
                && (playback.speech_during_playback
                    || (lane.audio_played && playback.speech_start.is_some()))
                && lane.substantive_overlap()
            {
                // Legacy threshold chain (shadow-only mode).
                playback.barged = true;
                playback.barged_at = Some(now);
            }
        }
        if playback.stop_playback_now(now) {
            break;
        }
        // Natural end: synthesis finished, everything queued, playout done.
        if synth_done && pending.is_empty() && playback.played_out(now) {
            natural_end = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // SHADOW divergence: did the fused decision-maker agree with what the
    // live paths actually did to this span? Disagreements are the tuning
    // signal — an echo-driven energy barge shows up as "actual cut, shadow
    // saw nothing"; an ignored comment as "shadow cut/yield, span played
    // out". One line per divergent span, content-free.
    if let Some(lane) = probe.as_deref().filter(|_| !floor_manager_owns()) {
        // Divergence is only meaningful in shadow-only mode — once promoted,
        // the decision IS the action.
        let actual_cut = playback.barged || playback.semantic.is_some();
        let shadow_cut = shadow_worst >= 2; // YieldAtBoundary or stronger
        if shadow_cut != actual_cut && !playback.cancelled {
            lane.status
                .floor_shadow_divergences
                .fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[aokie-plugin] floor shadow divergence: shadow {} vs actual {}",
                if shadow_cut { "cut/yield" } else { "continue" },
                if actual_cut { "cut" } else { "played out" },
            );
        }
    }
    // A cut span leaves the worker mid-stream: invalidate its epoch so it
    // aborts at the next chunk instead of synthesizing into the void.
    if playback.cancelled || playback.barged || playback.semantic.is_some() || !synth_done {
        synth.cancel();
    }
    if let Some(e) = synth_err.as_ref() {
        if playback.samples == 0 {
            eprintln!("[aokie-plugin] TTS synthesis failed: {e}");
        }
    }
    // §6.3 delivery truth: a cut span records the numbers needed to estimate
    // its audible prefix — what was queued, how long audio flowed, and (when
    // synthesis finished first) the exact synthesized total.
    let cut_est = if !natural_end && !playback.first && playback.samples > 0 {
        let sr = sample_rate.max(1) as u64;
        let queued_ms = playback.samples as u64 * 1000 / sr;
        let audible_ms = (playback.t_first.elapsed().as_millis() as u64).min(queued_ms);
        let synthesized_ms = if synth_done && synth_err.is_none() {
            Some(queued_ms + pending.len() as u64 * 1000 / sr)
        } else {
            None
        };
        Some(CutEstimate {
            audible_ms,
            queued_ms,
            synthesized_ms,
        })
    } else {
        None
    };
    let mut out = playback.into_outcome(text, sample_rate);
    out.cut_est = cut_est;
    out
}

/// The outcome of speaking one PLANNED utterance (a sequence of validated
/// [`crate::speech_plan::SpeechSpan`]s): the aggregated playback outcome,
/// the marker-free text of the whole plan, and the marker-free text of the
/// spans that actually PLAYED (what transcripts/history may record).
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) struct PlannedSpeech {
    pub(super) outcome: SpeakOutcome,
    /// Everything the plan intended to say (markers stripped).
    pub(super) text: String,
    /// §6.3 estimated-AUDIBLE text (markers stripped): full spans that played
    /// to their natural end, plus a CONSERVATIVE word-floored prefix of the
    /// cut span. What transcripts, history and the nudge may claim was heard.
    pub(super) played_text: String,
    /// §6.3 text SENT toward the phone (markers stripped): the full text of
    /// every span that produced audio, cut span included — the comparator for
    /// the echo guard and deterministic replay, where the flushed-but-echoed
    /// tail must still match.
    pub(super) sent_text: String,
}

/// Speak `raw_text` through the span planner: strips/validates any control
/// markup, slows + digit-expands phone-number/code runs, applies per-span
/// interrupt policy, and plays the spans in order. ONE pipeline for every
/// speech origin — the built-in agent, the greeting, flow/operator speech —
/// so pacing and duplex behaviour never depend on where words came from.
///
/// Stops early on a barge (after the barged span finishes its bounded
/// extension, if any) or an urgent control; the remaining spans are never
/// spoken (yield: the caller has the floor).
/// Phase 4: speak a FIXED announcement (a hold/resume line) to whoever is
/// currently active, plainly — no barge lane, no STT capture (these lines
/// are not up for interruption and the caller's audio during them is
/// discarded by the juggle). A hangup/reject arriving mid-line still cuts
/// it and is returned so the caller can honour it.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn speak_announcement(
    bt: &mut dyn crate::backend::RadioBackend,
    synth: &crate::synth::SynthHandle,
    text: &str,
    sample_rate: u16,
    pace: &crate::speech_plan::PaceState,
    protected_max_ms: u32,
    control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
) -> Option<CancelAction> {
    let mut probe = ControlProbe::new(control_rx, pending_controls);
    let _ = speak_planned(
        bt,
        synth,
        text,
        sample_rate,
        None,
        None,
        Some(&mut probe),
        pace,
        protected_max_ms,
        None,
        None,
    );
    probe.action.take()
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn speak_planned(
    bt: &mut dyn crate::backend::RadioBackend,
    synth: &crate::synth::SynthHandle,
    raw_text: &str,
    sample_rate: u16,
    mut aec: Option<&mut crate::aec::EchoCanceller>,
    barge_rms: Option<f32>,
    mut ctl: Option<&mut ControlProbe<'_>>,
    pace: &crate::speech_plan::PaceState,
    protected_max_ms: u32,
    mut probe: Option<&mut SttProbeLane<'_>>,
    // Forwarded to every span's tts_speak: only the first span can actually
    // wait (later spans start after the gate has long passed). See tts_speak.
    egress_hold: Option<std::time::Instant>,
) -> PlannedSpeech {
    use std::time::Duration;
    let spans = crate::speech_plan::plan_spans(raw_text, pace, protected_max_ms);
    let text = crate::speech_plan::clean_text(&spans);
    let mut outcome = SpeakOutcome {
        dur: Duration::ZERO,
        barged: false,
        captured_speech: Vec::new(),
        cancelled: false,
        commanded: None,
        cut_est: None,
    };
    let mut played: Vec<String> = Vec::new();
    let mut sent: Vec<String> = Vec::new();
    for span in &spans {
        let finish_extra = match span.policy {
            crate::speech_plan::InterruptPolicy::Yield => None,
            crate::speech_plan::InterruptPolicy::FinishSpan { max_extra_ms } => {
                Some(Duration::from_millis(max_extra_ms as u64))
            }
        };
        let out = tts_speak(
            bt,
            synth,
            &span.tts_text,
            sample_rate,
            aec.as_deref_mut(),
            barge_rms,
            ctl.as_deref_mut(),
            span.rate,
            finish_extra,
            probe.as_deref_mut(),
            egress_hold,
        );
        outcome.dur += out.dur;
        if out.dur > Duration::ZERO {
            sent.push(span.text.clone());
            match &out.cut_est {
                // Cut mid-span: claim only the conservative audible prefix
                // (§6.3) — overclaiming loses whatever the caller never heard
                // from the transcript, the history AND the nudge tail.
                Some(cut) => {
                    let (prefix, _uncertain) = estimate_audible_prefix(&span.text, span.rate, cut);
                    if !prefix.is_empty() {
                        played.push(prefix);
                    }
                }
                None => played.push(span.text.clone()),
            }
        }
        if !out.captured_speech.is_empty() {
            outcome
                .captured_speech
                .extend_from_slice(&out.captured_speech);
        }
        if out.commanded.is_some() {
            outcome.commanded = out.commanded;
        }
        if out.cancelled {
            outcome.cancelled = true;
            break;
        }
        if out.barged {
            // The span itself honoured its policy (yield or bounded finish);
            // everything AFTER it always yields — the caller has the floor.
            outcome.barged = true;
            break;
        }
    }
    PlannedSpeech {
        outcome,
        text,
        played_text: played.join(" "),
        sent_text: sent.join(" "),
    }
}

/// Execute an urgent control the [`ControlProbe`] caught mid-playback: note
/// the termination intent (so `call.ended` reads the right outcome), flush
/// the queued audio tail, and act on the phone. A failed radio action emits
/// the authoritative `control_failed` diagnostic against the operation id.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn perform_cancel_action(
    action: CancelAction,
    bt: &mut dyn crate::backend::RadioBackend,
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) {
    bt.flush_tx_audio();
    match action {
        CancelAction::Hangup { op } => {
            tracker.note_intent(crate::call_session::TerminationIntent::OperatorHangup);
            if let Err(e) = bt.hangup() {
                eprintln!("[aokie-plugin] mid-playback hangup failed: {e}");
                emit_control_failed(outbox, sink, tracker, "call.hangup", op.as_deref(), &e);
            }
        }
        CancelAction::Reject { op } => {
            tracker.note_intent(crate::call_session::TerminationIntent::OperatorReject);
            if let Err(e) = bt.reject_call() {
                eprintln!("[aokie-plugin] mid-playback reject failed: {e}");
                emit_control_failed(outbox, sink, tracker, "call.reject", op.as_deref(), &e);
            }
        }
    }
}
