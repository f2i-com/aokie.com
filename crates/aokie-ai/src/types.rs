//! Neutral conversation shape used across `LlmProvider` /
//! `SttProvider` / `TtsProvider`. The existing Gemma 4 module's
//! `PromptSegment` enum is more specific to that one model's chat
//! template; this is the shape every provider's adapter speaks.

use serde::{Deserialize, Serialize};

/// Role of a turn in a conversation. We deliberately don't model
/// arbitrary tool/function roles here — tool-use today is pure text
/// inside `<TOOL>...</TOOL>` envelopes (see `crate::calendar::tools`
/// and `crate::orders::tools`), so the LLM provider never has to know
/// about it. Providers with native tool-call APIs can still use them
/// internally; they just translate to text on the way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Model,
}

/// One turn in a conversation. Audio is per-turn rather than a
/// separate flat segment so a multimodal provider that supports
/// multi-turn audio (e.g. Gemma 4 with two consecutive caller
/// utterances) can be expressed without ambiguity. Today's call
/// pipeline only attaches audio to the latest user turn — adapters
/// for audio-incapable providers ignore the field entirely.
#[derive(Debug, Clone)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// Mono f32 PCM at 16 kHz, normalized to roughly [-1, 1]. Only
    /// meaningful when the provider's `LlmCapabilities::audio_input`
    /// is true; ignored otherwise.
    pub audio_16k_mono: Option<Vec<f32>>,
}

impl Turn {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            audio_16k_mono: None,
        }
    }

    pub fn model(text: impl Into<String>) -> Self {
        Self {
            role: Role::Model,
            text: text.into(),
            audio_16k_mono: None,
        }
    }

    pub fn user_with_audio(text: impl Into<String>, audio: Vec<f32>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            audio_16k_mono: Some(audio),
        }
    }
}
