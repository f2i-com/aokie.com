//! Generic adapter wrapping an ONNX-Runtime-GenAI-style on-device LLM
//! (today: the in-process module under `crate::runtimes::onnx_genai`). The
//! adapter is intentionally model-agnostic — id, capabilities, and
//! chat-template flavor flow in via `LlmProviderConfig` so swapping
//! the bundled model is a config edit, not a source change.
//!
//! The adapter shares the `Arc<Mutex<Option<...>>>` the existing
//! state singleton already holds (we share, not duplicate, the
//! model). Translation is request → segment list → underlying
//! `generate_stream{,_with_audio}`, with the blocking ONNX work
//! pushed onto `spawn_blocking`.
//!
//! Note: the underlying runtime type is still concrete in Phase 1 —
//! generalizing the *runtime* (so a different ONNX model could plug
//! in here without code changes) is the Phase 2 engine-registry
//! work, not part of this adapter.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::LlmProviderConfig;
use crate::llm::{LlmCapabilities, LlmError, LlmProvider, LlmRequest, TokenSink};
use crate::runtimes::onnx_genai::{OnnxGenAiRuntime, PromptSegment};
use crate::types::Role;

pub struct OnnxGenAiLlm {
    cfg: LlmProviderConfig,
    inner: Arc<Mutex<Option<OnnxGenAiRuntime>>>,
}

impl OnnxGenAiLlm {
    pub fn new(cfg: LlmProviderConfig, inner: Arc<Mutex<Option<OnnxGenAiRuntime>>>) -> Self {
        Self { cfg, inner }
    }

    /// Convert a neutral `LlmRequest` into the segment list the
    /// underlying module expects. `system` becomes a leading
    /// system-role text turn. Per-turn audio is placed according to
    /// `cfg.audio_inline_before_text`: when true, the Audio segment
    /// sits immediately before its owning text turn (Gemma chat
    /// template — `[Audio, Text(user)]` is one logical turn). When
    /// false, audio is appended at the end of the segment list. The
    /// underlying module today supports one audio splice per
    /// generation; if multiple turns carry audio we keep the last.
    ///
    /// `cfg.audio_input` is the operator-side kill switch. When false,
    /// every turn's audio is skipped regardless of whether the model
    /// supports multimodal input — the LLM sees only the STT
    /// transcript text. This is the correct path when the operator
    /// wants the bot to ground its replies on the deterministic STT
    /// output rather than Gemma's audio interpretation (which on
    /// phone-band SCO with weak signal sometimes mis-transcribes the
    /// caller and trips the calendar prompt's hard rules).
    fn build_segments(&self, req: &LlmRequest) -> Vec<PromptSegment> {
        let mut out: Vec<PromptSegment> = Vec::with_capacity(req.turns.len() + 2);
        if let Some(sys) = req.system.as_deref() {
            if !sys.is_empty() {
                out.push(PromptSegment::Text {
                    role: "system".to_string(),
                    text: sys.to_string(),
                });
            }
        }
        let audio_enabled = self.cfg.audio_input;
        let last_audio_turn = if audio_enabled {
            req.turns
                .iter()
                .enumerate()
                .rev()
                .find_map(|(i, t)| t.audio_16k_mono.as_ref().map(|_| i))
        } else {
            None
        };
        for (idx, turn) in req.turns.iter().enumerate() {
            let role = match turn.role {
                Role::User => "user",
                Role::Model => "model",
            };
            if self.cfg.audio_inline_before_text && Some(idx) == last_audio_turn {
                if let Some(audio) = &turn.audio_16k_mono {
                    out.push(PromptSegment::Audio(audio.clone()));
                }
            }
            out.push(PromptSegment::Text {
                role: role.to_string(),
                text: turn.text.clone(),
            });
        }
        if !self.cfg.audio_inline_before_text {
            if let Some(idx) = last_audio_turn {
                if let Some(audio) = req.turns[idx].audio_16k_mono.clone() {
                    out.push(PromptSegment::Audio(audio));
                }
            }
        }
        out
    }
}

#[async_trait]
impl LlmProvider for OnnxGenAiLlm {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> LlmCapabilities {
        LlmCapabilities {
            audio_input: self.cfg.audio_input,
            streaming: self.cfg.streaming,
            max_context_tokens: self.cfg.max_context_tokens,
        }
    }

    async fn generate(&self, req: LlmRequest) -> Result<String, LlmError> {
        // Internal fall-through: stream into a buffer and return it.
        // Going through generate_stream means the multimodal path
        // picks itself up; the no-op token sink is cheap.
        let buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let buf_for_sink = buf.clone();
        let sink: TokenSink = Box::new(move |chunk| {
            if let Ok(mut g) = buf_for_sink.lock() {
                g.push_str(chunk);
            }
            true
        });
        self.generate_stream(req, sink).await?;
        let final_text = buf.lock().map(|g| g.clone()).unwrap_or_default();
        Ok(final_text)
    }

    async fn generate_stream(
        &self,
        req: LlmRequest,
        on_token: TokenSink,
    ) -> Result<String, LlmError> {
        let inner = self.inner.clone();
        let max_tokens = req.max_tokens as usize;
        let temperature = req.temperature;
        let mut sink = on_token;
        let segments = self.build_segments(&req);
        let has_audio = segments
            .iter()
            .any(|s| matches!(s, PromptSegment::Audio(_)));

        let result: Result<String, LlmError> =
            tokio::task::spawn_blocking(move || -> Result<String, LlmError> {
                let mut guard = inner.blocking_lock();
                let model = guard
                    .as_mut()
                    .ok_or_else(|| LlmError::NotReady("ONNX-GenAI LLM not loaded".to_string()))?;
                let outcome = if has_audio {
                    model.generate_stream_with_audio(&segments, max_tokens, temperature, |chunk| {
                        sink(chunk)
                    })
                } else {
                    model.generate_stream(&segments, max_tokens, temperature, |chunk| sink(chunk))
                };
                outcome.map_err(LlmError::Generate)
            })
            .await
            .map_err(|join_err| {
                LlmError::Generate(format!("ONNX-GenAI task panicked: {}", join_err))
            })?;

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Turn;

    fn cfg(audio_inline: bool) -> LlmProviderConfig {
        LlmProviderConfig {
            id: "test".to_string(),
            display_name: "Test".to_string(),
            kind: "onnx-genai".to_string(),
            audio_input: true,
            streaming: true,
            max_context_tokens: Some(32_000),
            audio_inline_before_text: audio_inline,
            chat_template: None,
            base_url: String::new(),
            api_key: String::new(),
            secret_ref: None,
            model: String::new(),
            system_in_user_turn: false,
        }
    }

    fn segs_with(req: &LlmRequest, c: LlmProviderConfig) -> Vec<PromptSegment> {
        // build_segments only touches cfg, so we can construct a stub
        // adapter with an empty model slot and call straight into it.
        OnnxGenAiLlm::new(c, Arc::new(Mutex::new(None))).build_segments(req)
    }

    #[test]
    fn build_segments_inlines_audio_before_owning_text() {
        let req = LlmRequest::new(vec![
            Turn::user("hi"),
            Turn::model("hello there"),
            Turn::user_with_audio("see attached", vec![0.0; 16000]),
        ])
        .with_system("you are a receptionist");
        let segs = segs_with(&req, cfg(true));
        assert_eq!(segs.len(), 5);
        match &segs[0] {
            PromptSegment::Text { role, text } => {
                assert_eq!(role, "system");
                assert_eq!(text, "you are a receptionist");
            }
            _ => panic!("expected system text first"),
        }
        match &segs[3] {
            PromptSegment::Audio(v) => assert_eq!(v.len(), 16000),
            _ => panic!("expected audio at index 3"),
        }
        match &segs[4] {
            PromptSegment::Text { role, text } => {
                assert_eq!(role, "user");
                assert_eq!(text, "see attached");
            }
            _ => panic!("expected user-text at index 4"),
        }
    }

    #[test]
    fn build_segments_skips_empty_system() {
        let req = LlmRequest::new(vec![Turn::user("hi")]);
        let segs = segs_with(&req, cfg(true));
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn build_segments_only_carries_one_audio_segment() {
        let req = LlmRequest::new(vec![
            Turn::user_with_audio("first", vec![0.1; 8000]),
            Turn::user_with_audio("second", vec![0.2; 4000]),
        ]);
        let segs = segs_with(&req, cfg(true));
        let audio_count = segs
            .iter()
            .filter(|s| matches!(s, PromptSegment::Audio(_)))
            .count();
        assert_eq!(audio_count, 1);
        let audio = segs
            .iter()
            .find_map(|s| match s {
                PromptSegment::Audio(v) => Some(v),
                _ => None,
            })
            .unwrap();
        assert_eq!(audio.len(), 4000);
    }

    #[test]
    fn build_segments_appends_audio_when_inline_off() {
        // Templates that don't group audio inside the user turn —
        // audio lands at the end of the segment list.
        let req = LlmRequest::new(vec![
            Turn::user("hi"),
            Turn::user_with_audio("audio here", vec![0.0; 4000]),
        ]);
        let segs = segs_with(&req, cfg(false));
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0], PromptSegment::Text { .. }));
        assert!(matches!(segs[1], PromptSegment::Text { .. }));
        assert!(matches!(segs[2], PromptSegment::Audio(_)));
    }
}
