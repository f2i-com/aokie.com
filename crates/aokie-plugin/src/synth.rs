//! The synthesis lane of the voice feature, with no voice behind it.
//!
//! Aokie has no text-to-speech of its own: on the OAIY route OAIY Voice
//! speaks the replies and OAIY's speak mode says Aokie's fixed lines. The
//! radio's own speaking path (`radio::tts_speak`, which paces PCM into the
//! SCO link while it keeps draining the mic and honouring controls) is still
//! the one every line Aokie might say goes through, so it keeps this handle:
//! each span it begins fails at once with [`crate::voice::NO_OWN_SPEECH`],
//! which `tts_speak` reports as a span that produced no audio, exactly as a
//! failed engine did.
//!
//! Cancellation is an EPOCH: [`SynthHandle::begin`] bumps a counter and the
//! outcome is tagged with the epoch it belongs to; bumping it again
//! ([`SynthHandle::cancel`]) makes the radio side discard anything stale.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

/// The outcome of a span, tagged with its epoch.
pub enum SynthOut {
    /// PCM at the SCO rate. Nothing produces it any more: the radio side
    /// that plays it is the same path a failed span takes.
    #[allow(dead_code)]
    Frames { epoch: u64, pcm: Vec<i16> },
    /// The span finished (all frames sent). Never, now.
    #[allow(dead_code)]
    Done { epoch: u64 },
    Failed { epoch: u64, error: String },
}

/// Outcomes held at most (a span has one): a full channel only drops a stale
/// outcome, never blocks the radio thread.
const OUT_CHANNEL_DEPTH: usize = 8;

pub struct SynthHandle {
    out_tx: mpsc::SyncSender<SynthOut>,
    pub out_rx: mpsc::Receiver<SynthOut>,
    epoch: AtomicU64,
    /// Tests that drive the radio loop: nothing is ever answered.
    inert: bool,
}

impl SynthHandle {
    /// A handle that answers nothing at all, not even a failure. For tests
    /// that drive the radio loop.
    #[cfg(test)]
    pub(crate) fn inert() -> Self {
        let mut handle = Self::spawn();
        handle.inert = true;
        handle
    }

    pub fn spawn() -> Self {
        let (out_tx, out_rx) = mpsc::sync_channel::<SynthOut>(OUT_CHANNEL_DEPTH);
        Self {
            out_tx,
            out_rx,
            epoch: AtomicU64::new(0),
            inert: false,
        }
    }

    /// Start a span; returns its epoch. Implicitly cancels whatever came
    /// before (the epoch moves) and drains stale output, then fails at once:
    /// there is no voice to say it with.
    pub fn begin(&self) -> u64 {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        while self.out_rx.try_recv().is_ok() {}
        if !self.inert {
            let _ = self.out_tx.try_send(SynthOut::Failed {
                epoch,
                error: crate::voice::NO_OWN_SPEECH.to_string(),
            });
        }
        epoch
    }

    /// Abort the current span: everything still in flight reads as stale.
    pub fn cancel(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Call boundary: invalidate any in-flight span.
    pub fn reset_call(&self) {
        self.cancel();
        while self.out_rx.try_recv().is_ok() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every span fails at once, saying why, and a later span's outcome is
    /// never mistaken for an earlier one's.
    #[test]
    fn a_span_fails_at_once_with_no_voice_of_its_own() {
        let synth = SynthHandle::spawn();
        let first = synth.begin();
        let second = synth.begin();
        assert!(second > first);
        match synth.out_rx.try_recv() {
            Ok(SynthOut::Failed { epoch, error }) => {
                assert_eq!(epoch, second);
                assert!(error.contains("no speech of its own"));
            }
            _ => panic!("the span did not fail"),
        }
        assert!(synth.out_rx.try_recv().is_err(), "the first span's outcome was drained");
        let inert = SynthHandle::inert();
        inert.begin();
        assert!(inert.out_rx.try_recv().is_err());
    }
}
