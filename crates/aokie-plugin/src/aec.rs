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
        Self {
            aec: Aec::new(&cfg),
            frame,
            ref_buf: VecDeque::with_capacity(rate as usize),
            mic_acc: VecDeque::with_capacity(frame * 8),
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
    /// queued (Aokie silent) the mic passes through essentially unchanged.
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
            self.aec.cancel_echo(&rec, &echo, &mut clean);
            out.extend_from_slice(&clean);
        }
        out
    }

    /// Drop stale reference + mic residual (call on call end / SCO teardown).
    pub fn reset(&mut self) {
        self.ref_buf.clear();
        self.mic_acc.clear();
    }
}
