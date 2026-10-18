//! Call-audio helpers for the voice feature, and the one thing the radio needs
//! to know about speech: Aokie has none of its own.
//!
//! On the OAIY route OAIY Voice hears the caller and speaks the replies, and
//! OAIY's speak mode says Aokie's fixed lines (radio/oaiy_speak.rs). There is
//! no speech-to-text or text-to-speech engine in the plugin, no ONNX Runtime
//! and no speech model to download. The radio starts with both of its voice
//! slots saying so ([`preflight`]): a call that does not go to OAIY is never
//! answered into silence (auto-answer stays off and it rings through), and a
//! fixed line OAIY cannot say is not said.

/// Why there is no speech for a call that does not go to OAIY: said in the
/// log, in the voice status slots and in health.
pub const NO_OWN_SPEECH: &str =
    "Aokie has no speech of its own: OAIY Voice hears and speaks for calls that go to OAIY";

/// Resample i16 PCM at `from` Hz to 16 kHz mono f32.
pub fn to_f32_16k(samples: &[i16], from: u32) -> Vec<f32> {
    let f: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
    crate::speech_wire::resample_linear(&f, from, 16_000)
}

/// RMS amplitude of an i16 frame in i16 units (0..32767): the caller-activity
/// gate on the realtime lane.
pub fn frame_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

/// The radio's two voice slots at start: whether Aokie itself can hear
/// (STT) and speak (TTS). `None` would mean it can; with no speech of its
/// own both always say why not.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VoicePreflight {
    pub stt_error: Option<String>,
    pub tts_error: Option<String>,
}

/// Both halves are known unable to work: Aokie has no speech of its own.
pub fn preflight() -> VoicePreflight {
    VoicePreflight {
        stt_error: Some(format!("speech-to-text unavailable: {NO_OWN_SPEECH}")),
        tts_error: Some(format!("text-to-speech unavailable: {NO_OWN_SPEECH}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both slots say why: a call off the OAIY route rings through, and a
    /// fixed line goes to OAIY's speak mode or is not said.
    #[test]
    fn neither_half_of_speech_is_aokies_own() {
        let p = preflight();
        assert!(p
            .stt_error
            .as_deref()
            .is_some_and(|e| e.contains("no speech of its own")));
        assert!(p
            .tts_error
            .as_deref()
            .is_some_and(|e| e.contains("no speech of its own")));
    }

    #[test]
    fn frame_rms_measures_the_level() {
        assert_eq!(frame_rms(&[]), 0.0);
        assert_eq!(frame_rms(&[100, -100, 100, -100]), 100.0);
    }
}
