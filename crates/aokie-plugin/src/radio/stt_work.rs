//! The speech-to-text lane's messages. Aokie has no speech-to-text of its
//! own (OAIY Voice hears the caller on the OAIY route), so the worker that
//! receives these answers none of them (run_loop.rs); the lanes that hand
//! them over are the ones a call off the OAIY route would take, which rings
//! through instead.

#[allow(unused_imports)]
use super::*;

// (Nothing reads a job's fields: there is no engine to transcribe it.)
#[cfg(feature = "voice")]
#[allow(dead_code)]
pub(super) enum SttWork {
    /// One finished caller utterance, stamped with the call it belongs to
    /// (audit C-05): a result for call A can never be attributed to call B.
    Utterance {
        generation: u64,
        utterance: u32,
        samples: Vec<f32>,
    },
    /// A snapshot of the caller's overlap speech while Aokie is talking,
    /// answered (were there an engine) on the dedicated probe channel, its
    /// `lane` echoed back so each consumer drops foreign results.
    Probe {
        generation: u64,
        lane: u32,
        samples: Vec<f32>,
    },
    ResetCall,
}

/// A finished transcription, still carrying the identity of the call whose
/// audio produced it.
#[cfg(feature = "voice")]
pub(super) struct SttResult {
    pub(super) generation: u64,
    pub(super) utterance: u32,
    pub(super) text: String,
}
