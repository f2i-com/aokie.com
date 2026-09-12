//! Acoustic echo cancellation for full-duplex / barge-in (voice feature).
//!
//! Wraps `aec-rs` (bundled speexdsp). Feed Aokie's outbound TTS as the echo
//! REFERENCE, and cancel it from the inbound SCO mic, so the receptionist reacts
//! only to the caller — not to its own voice echoing back over the phone link.
//! That clean signal is what lets Aokie keep listening WHILE it speaks (barge-in)
//! without transcribing itself.
//!
//! Single-threaded: the radio loop feeds the reference (in the TTS send path) and
//! processes the mic (in the audio-drain path) on the same thread, so no locking.

use aec_rs::{Aec, AecConfig};
use std::collections::VecDeque;

pub struct EchoCanceller {
    aec: Aec,
    frame: usize,           // samples per 10 ms frame at `rate`
    ref_buf: VecDeque<i16>, // outbound TTS reference, aligned FIFO with the mic
    mic_acc: VecDeque<i16>, // inbound mic accumulator for frame alignment
    raw_floor: f32,         // residual guard while the echo reference is active
    reference_tail_frames: usize, // protect the filter's 100 ms echo tail
}

/// Default RAW-input silence floor. Live call 066d2237 (first sherpa/Piper
/// call): with the phone transmitting DIGITAL SILENCE (raw rms 0–2) and a hot
/// TTS reference queued, speexdsp's output was the negative learned-filter
/// response — 250–2900 ms of "caller speech" the line never carried, which
/// seeded phantom turns and cut live replies. Real speech on the same phone
/// measured rms 390–1353, so a floor of 120 separates them with wide margin;
/// phones that DO put energy on the line (comfort noise, acoustic echo) pass
/// the floor and behave exactly as before — the AEC + capture gates decide.
const RAW_SILENCE_FLOOR: f32 = 120.0;

fn frame_rms(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / frame.len() as f64).sqrt() as f32
}

impl EchoCanceller {
    /// Build a canceller for the negotiated SCO rate (8 kHz CVSD / 16 kHz mSBC).
    pub fn new(rate: u32) -> Self {
        let frame = (rate as usize / 100).max(80); // 10 ms
        let cfg = AecConfig {
            frame_size: frame,
            filter_length: (rate as i32 / 10).max(800), // ~100 ms echo tail
            sample_rate: rate,
            enable_preprocess: true,
        };
        // AOKIE_CAPTURE_RAW_FLOOR overrides the silence floor; 0 disables the
        // guard entirely. Read per construction (one canceller per SCO/call).
        let raw_floor = std::env::var("AOKIE_CAPTURE_RAW_FLOOR")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(RAW_SILENCE_FLOOR);
        Self {
            aec: Aec::new(&cfg),
            frame,
            ref_buf: VecDeque::with_capacity(rate as usize),
            mic_acc: VecDeque::with_capacity(frame * 8),
            raw_floor,
            reference_tail_frames: 0,
        }
    }

    /// Append outbound TTS samples (exactly what's sent to SCO-TX) as the echo
    /// reference. Bounded so a runaway can't grow unbounded.
    pub fn feed_reference(&mut self, samples: &[i16]) {
        self.ref_buf.extend(samples);
        let cap = self.frame * 300; // ~3 s
        while self.ref_buf.len() > cap {
            self.ref_buf.pop_front();
        }
    }

    /// Echo-cancel a captured mic chunk → cleaned PCM (a multiple of the frame
    /// size; sub-frame leftovers stash for the next call). With no reference
    /// queued and its 100 ms tail expired, the mic passes through unchanged.
    pub fn process_capture(&mut self, mic: &[i16]) -> Vec<i16> {
        if mic.is_empty() {
            return Vec::new();
        }
        self.mic_acc.extend(mic);
        let frames = self.mic_acc.len() / self.frame;
        if frames == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(frames * self.frame);
        let mut rec = vec![0i16; self.frame];
        let mut echo = vec![0i16; self.frame];
        let mut clean = vec![0i16; self.frame];
        for _ in 0..frames {
            for s in rec.iter_mut() {
                *s = self.mic_acc.pop_front().unwrap_or(0);
            }
            for s in echo.iter_mut() {
                *s = self.ref_buf.pop_front().unwrap_or(0);
            }
            // The filter must still adapt (and the ref FIFO stay aligned)
            // even for frames the residual guard silences below.
            self.aec.cancel_echo(&rec, &echo, &mut clean);
            let has_reference = echo.iter().any(|&sample| sample != 0);
            let echo_active = has_reference || self.reference_tail_frames > 0;
            if has_reference {
                self.reference_tail_frames = 10;
            } else {
                self.reference_tail_frames = self.reference_tail_frames.saturating_sub(1);
            }
            if !echo_active {
                // When Aokie is silent there is no echo to remove. Applying
                // the residual gate here erased quiet consonants: a controlled
                // Parakeet test changed "Lance" to "Lan". Keep the original
                // microphone samples, including soft word endings and onsets.
                out.extend_from_slice(&rec);
            } else if self.raw_floor > 0.0 && frame_rms(&rec) < self.raw_floor {
                // Residual guard: the RAW frame carried no meaningful audio,
                // so anything in `clean` was generated inside the filter —
                // emit true silence instead of a ghost of our own voice.
                out.extend(std::iter::repeat(0i16).take(self.frame));
            } else {
                out.extend_from_slice(&clean);
            }
        }
        out
    }

    /// Drop stale reference + mic residual (call on call end / SCO teardown).
    pub fn reset(&mut self) {
        self.ref_buf.clear();
        self.mic_acc.clear();
        self.reference_tail_frames = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_assistant_preserves_quiet_consonants_across_capture_chunks() {
        let mut ec = EchoCanceller::new(8000);
        // Speech falls from a vowel to a quiet unvoiced ending below the old
        // residual floor. Split into the actual 3 ms CVSD packet size.
        let input: Vec<i16> = (0..1600).map(|i| {
            let amplitude = if i < 800 { 2000.0 } else { 75.0 };
            ((i as f32 * 0.61).sin() * amplitude) as i16
        }).collect();
        let output: Vec<i16> = input.chunks(24)
            .flat_map(|chunk| ec.process_capture(chunk)).collect();
        assert_eq!(output, input, "idle AEC must not gate the caller's phonemes");
    }

    #[test]
    fn residual_guard_stays_active_through_the_reference_tail() {
        let mut ec = EchoCanceller::new(8000);
        ec.feed_reference(&vec![3000; 80]);
        assert_eq!(ec.process_capture(&vec![2; 80]), vec![0; 80]);
        for _ in 0..10 {
            assert_eq!(ec.process_capture(&vec![2; 80]), vec![0; 80]);
        }
        assert_eq!(ec.process_capture(&vec![2; 80]), vec![2; 80]);
        ec.reset();
        assert_eq!(ec.process_capture(&vec![75; 80]), vec![75; 80]);
    }

    /// A hot reference against a digitally-silent mic must produce SILENCE —
    /// the exact phantom-capture shape from live call 066d2237: speexdsp's
    /// residual (−W·reference) on a zero mic frame used to cross the 350-RMS
    /// capture gate and mint caller turns out of nothing.
    #[test]
    fn silent_mic_with_hot_reference_outputs_silence() {
        let mut ec = EchoCanceller::new(16_000);
        // Hot wideband reference (Piper-class loudness).
        let reference: Vec<i16> = (0..16_000)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                ((t * 700.0 * std::f32::consts::TAU).sin() * 24_000.0
                    + (t * 2_900.0 * std::f32::consts::TAU).sin() * 9_000.0) as i16
            })
            .collect();
        // Interleave feed/process the way the paced loop does (20 ms chunks).
        let chunk = 320;
        let mut max_rms = 0f32;
        for c in reference.chunks(chunk) {
            ec.feed_reference(c);
            let cleaned = ec.process_capture(&vec![0i16; c.len()]);
            for f in cleaned.chunks(ec.frame) {
                max_rms = max_rms.max(frame_rms(f));
            }
        }
        assert_eq!(max_rms, 0.0, "residual guard must silence a silent mic");
    }

    /// Real caller speech (well above the floor) passes through — the guard
    /// only ever fires on frames the line itself reported as silent.
    #[test]
    fn real_speech_passes_the_raw_floor() {
        let mut ec = EchoCanceller::new(16_000);
        let speech: Vec<i16> = (0..1600)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                ((t * 220.0 * std::f32::consts::TAU).sin() * 3_000.0) as i16
            })
            .collect();
        // No reference queued (bot silent): mic should pass essentially intact.
        let cleaned = ec.process_capture(&speech);
        assert_eq!(cleaned.len(), speech.len());
        let rms = frame_rms(&cleaned);
        assert!(
            rms > 400.0,
            "speech above the floor must not be silenced (rms {rms})"
        );
    }

    /// Sub-floor guard math: rms of a quiet frame sits under the default
    /// floor, a normal-speech frame sits far over it.
    #[test]
    fn raw_floor_separates_silence_from_speech() {
        let quiet = vec![2i16; 160];
        let speech: Vec<i16> = (0..160)
            .map(|i| ((i as f32 * 0.3).sin() * 1_000.0) as i16)
            .collect();
        assert!(frame_rms(&quiet) < RAW_SILENCE_FLOOR);
        assert!(frame_rms(&speech) > RAW_SILENCE_FLOOR);
    }
}
