//! Acoustic echo cancellation for the bot's outbound voice.
//!
//! Phones loop our SCO TX (bot speech) back through the mic at
//! attenuated levels (~0.05-0.15 peak in float). Whisper Large-v3 then
//! transcribes the echo as common filler ("Thank you.", "Yeah.",
//! "Okay.") and the turn dispatcher acts on it. Half-duplex muting
//! (default) sidesteps this by dropping mic samples while TTS plays,
//! at the cost of barge-in. The `AecBridge` wrapper here uses speexdsp
//! (via `aec-rs`) to subtract the bot's outbound voice from the mic
//! stream so genuine barge-in still works.
//!
//! The bridge is a thin wrapper around an `Aec` plus a reference-signal
//! ring buffer:
//!
//! - `feed_reference` is called from the TTS send path with the exact
//!   PCM bytes we hand to `BluetoothManager::send_audio`. Samples are
//!   appended to the ring buffer; we cap it at five seconds of audio
//!   so a fire-and-forget TTS burst can't grow the buffer unbounded.
//! - `process_capture` is called from the audio-drain loop with each
//!   inbound mSBC-decoded mic chunk. The same number of reference
//!   samples are popped from the front of the buffer (oldest first —
//!   matches the SCO TX FIFO consumption order) and the speexdsp
//!   echo canceller runs frame-by-frame at 160 samples (10 ms @ 16 kHz).
//!
//! Time alignment is implicit: the SCO TX queue and our ref buffer
//! both grow with TTS pushes and shrink at 16 kHz steady (TX consumes
//! to the radio, mic captures from the radio). As long as both ring
//! buffers start empty when the SCO link comes up and both are drained
//! at the same rate, the front of the ref buffer corresponds to "what
//! is currently being heard by the mic". The speexdsp adaptive filter
//! has a 100 ms tail (`filter_length: 1600`) which absorbs small
//! mismatches caused by SCO TX jitter and acoustic delay.

use std::collections::VecDeque;
use std::sync::Mutex;

use aec_rs::{Aec, AecConfig};

/// Sample rate the bridge is configured for, in Hz. Exposed as `u16`
/// to match `AudioFrame::sample_rate` so the call site can gate without
/// casting. Anything other than mSBC (16 kHz) bypasses the AEC and
/// falls back to half-duplex muting.
// AEC is consumed only from the Windows BT call body in
// `bluetooth_commands.rs` — the Linux developer-preview build
// stubs the call body, so cargo flags every const + the struct +
// impl methods here as dead. Suppress on Linux without dropping
// the implementation; tests below stay live so the speexdsp
// integration is still exercised on the Linux Rust job.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const SUPPORTED_SAMPLE_RATE: u16 = 16_000;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const FRAME_SIZE: usize = 160; // 10 ms @ 16 kHz mSBC
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const FILTER_LENGTH: i32 = 1600; // 100 ms echo tail
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const MAX_REF_BUFFER_SAMPLES: usize = 80_000; // 5 s safety cap

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub struct AecBridge {
    aec: Mutex<Aec>,
    ref_buffer: Mutex<VecDeque<i16>>,
    // Residual mic samples that didn't add up to a full FRAME_SIZE on
    // the last call. speexdsp's adaptive filter expects fixed-size
    // frames; feeding it short, zero-padded chunks (mSBC delivers 240-
    // sample chunks which don't divide evenly into 160) corrupts the
    // filter coefficients and the canceller diverges within a few
    // hundred frames. We accumulate here and only ever feed full frames.
    mic_accumulator: Mutex<VecDeque<i16>>,
}

// `Aec` holds raw pointers to speexdsp state. The underlying C library
// is single-threaded per state, which the inner Mutex enforces — so the
// wrapper as a whole is safe to share across the TTS-send and audio-
// drain tasks via Arc.
unsafe impl Send for AecBridge {}
unsafe impl Sync for AecBridge {}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl AecBridge {
    pub fn new() -> Self {
        let cfg = AecConfig {
            frame_size: FRAME_SIZE,
            filter_length: FILTER_LENGTH,
            sample_rate: SUPPORTED_SAMPLE_RATE as u32,
            enable_preprocess: true,
        };
        Self {
            aec: Mutex::new(Aec::new(&cfg)),
            ref_buffer: Mutex::new(VecDeque::with_capacity(MAX_REF_BUFFER_SAMPLES)),
            mic_accumulator: Mutex::new(VecDeque::with_capacity(FRAME_SIZE * 4)),
        }
    }

    /// Append outbound TTS samples to the reference ring. Caller must
    /// pass the exact i16 PCM that goes to SCO TX, at 16 kHz.
    pub fn feed_reference(&self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let mut buf = match self.ref_buffer.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for &s in samples {
            buf.push_back(s);
        }
        while buf.len() > MAX_REF_BUFFER_SAMPLES {
            buf.pop_front();
        }
    }

    /// Drop pending reference samples and any partially-accumulated mic
    /// frame. Call when SCO tears down so the next call's first mic
    /// frames don't get echo-cancelled against stale reference samples
    /// or stale mic residual from the previous call.
    pub fn reset(&self) {
        if let Ok(mut buf) = self.ref_buffer.lock() {
            buf.clear();
        }
        if let Ok(mut acc) = self.mic_accumulator.lock() {
            acc.clear();
        }
    }

    /// Run echo cancellation over a captured mic chunk. Output length
    /// is a multiple of `FRAME_SIZE` (10 ms @ 16 kHz) — leftover samples
    /// that don't fill a full frame are stashed and emitted on a
    /// subsequent call. Caller may receive fewer samples than supplied
    /// (or zero) on any given call; over many calls the totals balance
    /// modulo a < FRAME_SIZE residual that flushes on `reset()`.
    ///
    /// If the reference ring is short of samples for a given frame the
    /// missing slots are filled with zeros — a chunk arriving before
    /// any TTS push falls through unchanged.
    pub fn process_capture(&self, mic: &[i16]) -> Vec<i16> {
        if mic.is_empty() {
            return Vec::new();
        }
        let mut acc = match self.mic_accumulator.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut buf = match self.ref_buffer.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let aec = match self.aec.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        acc.extend(mic);

        let frames = acc.len() / FRAME_SIZE;
        if frames == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(frames * FRAME_SIZE);
        let mut rec_frame = vec![0i16; FRAME_SIZE];
        let mut echo_frame = vec![0i16; FRAME_SIZE];
        let mut out_frame = vec![0i16; FRAME_SIZE];

        for _ in 0..frames {
            for slot in rec_frame.iter_mut() {
                *slot = acc.pop_front().unwrap_or(0);
            }
            for slot in echo_frame.iter_mut() {
                *slot = buf.pop_front().unwrap_or(0);
            }
            aec.cancel_echo(&rec_frame, &echo_frame, &mut out_frame);
            out.extend_from_slice(&out_frame);
        }
        out
    }
}

impl Default for AecBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_capture_returns_full_frames_only() {
        let bridge = AecBridge::new();
        let mic = vec![100i16; 240];
        let out = bridge.process_capture(&mic);
        // 240 samples → 1 full FRAME_SIZE (160) emitted, 80 stashed.
        assert_eq!(out.len(), FRAME_SIZE);
        // Subsequent call carrying just enough to complete the next
        // frame should emit another FRAME_SIZE.
        let out2 = bridge.process_capture(&vec![100i16; 80]);
        assert_eq!(out2.len(), FRAME_SIZE);
    }

    #[test]
    fn feed_reference_caps_at_five_seconds() {
        let bridge = AecBridge::new();
        bridge.feed_reference(&vec![1i16; MAX_REF_BUFFER_SAMPLES + 1000]);
        let buf = bridge.ref_buffer.lock().unwrap();
        assert_eq!(buf.len(), MAX_REF_BUFFER_SAMPLES);
    }

    #[test]
    fn reset_clears_reference_and_mic_accumulator() {
        let bridge = AecBridge::new();
        bridge.feed_reference(&vec![1i16; 1000]);
        // Stash partial mic frame so we can verify reset clears it too.
        let _ = bridge.process_capture(&vec![100i16; 50]);
        bridge.reset();
        assert_eq!(bridge.ref_buffer.lock().unwrap().len(), 0);
        assert_eq!(bridge.mic_accumulator.lock().unwrap().len(), 0);
    }

    #[test]
    fn process_capture_stashes_partial_chunk() {
        let bridge = AecBridge::new();
        bridge.feed_reference(&vec![0i16; 1000]);
        let mic = vec![100i16; 50]; // shorter than FRAME_SIZE
        let out = bridge.process_capture(&mic);
        // No full frame yet — caller gets nothing this call.
        assert_eq!(out.len(), 0);
        assert_eq!(bridge.mic_accumulator.lock().unwrap().len(), 50);
    }

    #[test]
    fn supported_sample_rate_is_16khz() {
        // Constants the call site uses to gate AEC on/off must agree
        // with what the bridge actually configures speexdsp for.
        assert_eq!(SUPPORTED_SAMPLE_RATE, 16_000_u16);
    }
}
