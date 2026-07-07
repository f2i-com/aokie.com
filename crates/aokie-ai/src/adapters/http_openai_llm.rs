//! OpenAI-compatible HTTP LLM adapter.
//!
//! Hits any provider that speaks the `/v1/chat/completions` shape:
//! OpenAI proper, LM Studio, Ollama (with `/v1` prefix), llama.cpp's
//! built-in server, vLLM, OpenRouter, together.ai, ... — the user
//! puts the base URL + model + (optional) API key in the provider
//! config and the trait surface upstream doesn't change.
//!
//! Streaming is via Server-Sent Events: the response is a stream of
//! `data: { delta: { content: "..." } }` lines terminated by
//! `data: [DONE]`. We accumulate raw bytes, slice at `\n\n` event
//! boundaries (UTF-8-safe because the delimiter is two ASCII bytes),
//! and feed each `delta.content` straight into the trait's
//! `TokenSink`. Returning `false` from the sink stops at the next
//! chunk.
//!
//! Non-streaming `generate()` posts `stream:false` and reads the
//! whole response — cheaper for short / classifier-style passes.
//! Both paths share `build_messages` so the chat-template is
//! consistent.
//!
//! What this adapter doesn't do (by design):
//! - Audio input — `/v1/chat/completions` is text-only on most
//!   providers; the multimodal realtime API is a different surface
//!   we'll handle as a separate adapter.
//! - Tool / function calling — today's pipeline does tool use
//!   inside `<TOOL>...</TOOL>` envelopes in plain text, so the
//!   trait stays narrow.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::LlmProviderConfig;
use crate::llm::{LlmCapabilities, LlmError, LlmProvider, LlmRequest, TokenSink};
use crate::types::Role;

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    /// llama.cpp's `llama-server` accepts arbitrary kwargs that get
    /// passed to the Jinja chat template. Qwen3.5 reads
    /// `enable_thinking` here to skip its reasoning_content block —
    /// without that, every reply burns the full token budget on
    /// reasoning the streaming content sink never sees. Models whose
    /// templates don't reference the field ignore it cleanly.
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    chat_template_kwargs: serde_json::Value,
}

#[derive(Deserialize, Default)]
struct ChatDelta {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct ChatMessageOut {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    #[serde(default)]
    message: Option<ChatMessageOut>,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

pub struct HttpOpenAiLlm {
    cfg: LlmProviderConfig,
    client: Client,
}

impl HttpOpenAiLlm {
    pub fn new(cfg: LlmProviderConfig) -> Self {
        // Generous default timeout — local servers (LM Studio, Ollama)
        // can take 30 s+ on a cold start. Streaming requests don't
        // hit this because each chunk resets the read timer.
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client build");
        Self { cfg, client }
    }

    /// Build the OpenAI `messages[]` array. When `fold_system` is true
    /// the system prompt is merged into the first user turn instead of
    /// being sent as its own `{role:"system"}` message — see
    /// `LlmProviderConfig::system_in_user_turn` for why (small models
    /// that ignore the system role still honour persona text in the
    /// user turn).
    fn build_messages(req: &LlmRequest, fold_system: bool) -> Vec<ChatMessage<'static>> {
        let sys = req.system.as_deref().filter(|s| !s.is_empty());
        let mut out = Vec::with_capacity(req.turns.len() + 1);
        // Standard path: system rides as its own role at the head.
        if let (Some(sys), false) = (sys, fold_system) {
            out.push(ChatMessage {
                role: "system",
                content: sys.to_string(),
            });
        }
        // Fold path: hold the system text until the first user turn,
        // then prepend it to that turn's content.
        let mut pending_system = if fold_system { sys } else { None };
        for turn in &req.turns {
            let role = match turn.role {
                Role::User => "user",
                // OpenAI calls model turns "assistant"; this is the
                // single role-name divergence between the trait and
                // the wire.
                Role::Model => "assistant",
            };
            let content = match (role, pending_system.take()) {
                ("user", Some(sys)) => format!("{}\n\n{}", sys, turn.text),
                // take() on a non-user turn would drop the system; only
                // consume it on a user turn, so restore it otherwise.
                (_, taken) => {
                    pending_system = taken;
                    turn.text.clone()
                }
            };
            out.push(ChatMessage { role, content });
        }
        // Fold requested but there was no user turn to attach to (e.g.
        // a greeting generation carrying only model context) — surface
        // the persona as a leading user message so it isn't dropped.
        if let Some(sys) = pending_system {
            out.insert(
                0,
                ChatMessage {
                    role: "user",
                    content: sys.to_string(),
                },
            );
        }
        out
    }

    /// llama-server (the only `kind` we expect to honour custom
    /// chat_template_kwargs) gets `enable_thinking=false` to short-
    /// circuit Qwen3's reasoning_content burst. Other kinds get
    /// `Value::Null`, which serialises out (skip_serializing_if). The
    /// directive is harmless for non-Qwen models loaded into
    /// llama-server — their Jinja templates just don't reference the
    /// field.
    fn chat_template_kwargs(&self) -> serde_json::Value {
        if self.cfg.kind == "llama-server" {
            serde_json::json!({ "enable_thinking": false })
        } else {
            serde_json::Value::Null
        }
    }

    fn endpoint(&self) -> Result<String, LlmError> {
        if self.cfg.base_url.is_empty() {
            return Err(LlmError::NotReady(
                "openai-http base_url is empty — set it in ai_providers.json".to_string(),
            ));
        }
        let trimmed = self.cfg.base_url.trim_end_matches('/');
        // Some users paste the full /v1/chat/completions URL; tolerate
        // both shapes so the config field doesn't have to be precise.
        if trimmed.ends_with("/chat/completions") {
            Ok(trimmed.to_string())
        } else if trimmed.ends_with("/v1") {
            Ok(format!("{}/chat/completions", trimmed))
        } else {
            Ok(format!("{}/v1/chat/completions", trimmed))
        }
    }

    fn auth_header(&self) -> Option<String> {
        // Trim because users routinely paste keys with a trailing
        // newline from a terminal copy. An untrimmed `Bearer sk-…\n`
        // header surfaces as a 401 with no obvious cause.
        let configured = self.cfg.api_key.trim();
        if !configured.is_empty() {
            return Some(format!("Bearer {}", configured));
        }
        // R3-#11: when the kind is `llama-server` and an in-process
        // sidecar is running, fall back to the per-session token the
        // sidecar was spawned with. That closes the "any local
        // process can hit 127.0.0.1:<port>" gap the reviewer flagged
        // — without this, an empty api_key meant `Authorization`
        // wasn't sent, and llama-server happily served any caller.
        // For other kinds (`openai-http` pointed at a real cloud),
        // an empty key continues to mean "anonymous" — that's the
        // shape Ollama / LM Studio / public test endpoints expect.
        if self.cfg.kind == "llama-server" {
            if let Some(token) = crate::sidecars::llama_server::current_session_token() {
                return Some(format!("Bearer {}", token));
            }
        }
        None
    }

    fn require_model(&self) -> Result<&str, LlmError> {
        if self.cfg.model.is_empty() {
            Err(LlmError::NotReady(
                "openai-http model is empty — set it in ai_providers.json".to_string(),
            ))
        } else {
            Ok(&self.cfg.model)
        }
    }
}

#[async_trait]
impl LlmProvider for HttpOpenAiLlm {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> LlmCapabilities {
        LlmCapabilities {
            // /v1/chat/completions is text-only for the providers
            // this adapter targets. Audio LLMs (gpt-4o realtime, etc.)
            // are a separate trait impl.
            audio_input: false,
            streaming: true,
            max_context_tokens: self.cfg.max_context_tokens,
        }
    }

    async fn generate(&self, req: LlmRequest) -> Result<String, LlmError> {
        let model = self.require_model()?;
        let body = ChatRequest {
            model,
            messages: Self::build_messages(&req, self.cfg.system_in_user_turn),
            stream: false,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            stop: req.stop.clone(),
            chat_template_kwargs: self.chat_template_kwargs(),
        };
        let json_body = serde_json::to_string(&body)
            .map_err(|e| LlmError::Generate(format!("serialize request: {}", e)))?;
        let mut http = self
            .client
            .post(self.endpoint()?)
            .header("Content-Type", "application/json")
            .body(json_body);
        if let Some(auth) = self.auth_header() {
            http = http.header("Authorization", auth);
        }
        let resp = http
            .send()
            .await
            .map_err(|e| LlmError::Generate(format!("HTTP send: {}", e)))?;
        let status = resp.status();
        let body_text = resp
            .text()
            .await
            .map_err(|e| LlmError::Generate(format!("HTTP body: {}", e)))?;
        if !status.is_success() {
            return Err(LlmError::Generate(format!(
                "HTTP {}: {}",
                status,
                body_text.chars().take(200).collect::<String>()
            )));
        }
        let parsed: ChatResponse = serde_json::from_str(&body_text).map_err(|e| {
            LlmError::Generate(format!(
                "parse: {} (body={:?})",
                e,
                body_text.chars().take(200).collect::<String>()
            ))
        })?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.map(|m| m.content))
            .unwrap_or_default();
        Ok(content)
    }

    async fn generate_stream(
        &self,
        req: LlmRequest,
        on_token: TokenSink,
    ) -> Result<String, LlmError> {
        let model = self.require_model()?;
        let body = ChatRequest {
            model,
            messages: Self::build_messages(&req, self.cfg.system_in_user_turn),
            stream: true,
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            stop: req.stop.clone(),
            chat_template_kwargs: self.chat_template_kwargs(),
        };
        let json_body = serde_json::to_string(&body)
            .map_err(|e| LlmError::Generate(format!("serialize request: {}", e)))?;
        let mut http = self
            .client
            .post(self.endpoint()?)
            .header("Content-Type", "application/json")
            .body(json_body);
        if let Some(auth) = self.auth_header() {
            http = http.header("Authorization", auth);
        }
        let resp = http
            .send()
            .await
            .map_err(|e| LlmError::Generate(format!("HTTP send: {}", e)))?;
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Generate(format!(
                "HTTP {}: {}",
                status,
                body_text.chars().take(200).collect::<String>()
            )));
        }
        let mut sink = on_token;
        let mut full = String::new();
        let mut byte_buf: Vec<u8> = Vec::with_capacity(4096);
        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk.map_err(|e| LlmError::Generate(format!("stream chunk: {}", e)))?;
            byte_buf.extend_from_slice(&bytes);
            // Pull complete SSE events ("\n\n"-terminated) out of the
            // buffer. Two-byte ASCII delimiter is UTF-8-safe — multi-byte
            // chars never split across the delimiter so we can decode
            // each event in isolation.
            loop {
                let Some(idx) = byte_buf.windows(2).position(|w| w == b"\n\n") else {
                    break;
                };
                let event_bytes = &byte_buf[..idx];
                // Lossy decode: SSE events from OpenAI-compatible
                // servers are always UTF-8, but treating a stray
                // invalid byte as `default()` (empty string) silently
                // drops the entire event. `from_utf8_lossy` keeps the
                // valid prefix + replacement char so json parsing
                // either still succeeds or fails loudly.
                let event = String::from_utf8_lossy(event_bytes).into_owned();
                byte_buf.drain(..idx + 2);
                for line in event.lines() {
                    let line = line.trim_start();
                    if !line.starts_with("data:") {
                        continue;
                    }
                    let data = line["data:".len()..].trim();
                    if data == "[DONE]" {
                        return Ok(full);
                    }
                    if let Ok(parsed) = serde_json::from_str::<ChatResponse>(data) {
                        for choice in parsed.choices {
                            let delta = choice.delta.content;
                            if delta.is_empty() {
                                continue;
                            }
                            let cont = sink(&delta);
                            full.push_str(&delta);
                            if !cont {
                                return Err(LlmError::Interrupted);
                            }
                        }
                    }
                }
            }
        }
        Ok(full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Turn;

    fn cfg(base_url: &str, model: &str, api_key: &str) -> LlmProviderConfig {
        LlmProviderConfig {
            id: "test-http".to_string(),
            display_name: "Test HTTP".to_string(),
            kind: "openai-http".to_string(),
            audio_input: false,
            streaming: true,
            max_context_tokens: None,
            audio_inline_before_text: false,
            chat_template: None,
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            secret_ref: None,
            model: model.to_string(),
            system_in_user_turn: false,
        }
    }

    #[test]
    fn build_messages_default_keeps_system_role() {
        let req = LlmRequest::new(vec![Turn::user("how much for a mow?")])
            .with_system("You are the receptionist for Jake's Mowing.");
        let msgs = HttpOpenAiLlm::build_messages(&req, false);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "You are the receptionist for Jake's Mowing.");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "how much for a mow?");
    }

    #[test]
    fn build_messages_fold_merges_system_into_first_user_turn() {
        let req = LlmRequest::new(vec![
            Turn::user("first"),
            Turn::model("hi"),
            Turn::user("second"),
        ])
        .with_system("PERSONA");
        let msgs = HttpOpenAiLlm::build_messages(&req, true);
        // No standalone system message; persona rides the FIRST user turn only.
        assert!(msgs.iter().all(|m| m.role != "system"));
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "PERSONA\n\nfirst");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content, "second");
    }

    #[test]
    fn build_messages_fold_with_no_user_turn_emits_leading_user() {
        // Greeting-style request: only model context, no user turn.
        let req = LlmRequest::new(vec![Turn::model("(greeting)")]).with_system("PERSONA");
        let msgs = HttpOpenAiLlm::build_messages(&req, true);
        assert!(msgs.iter().all(|m| m.role != "system"));
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "PERSONA");
    }

    #[test]
    fn build_messages_fold_noop_when_no_system() {
        let req = LlmRequest::new(vec![Turn::user("hello")]);
        let msgs = HttpOpenAiLlm::build_messages(&req, true);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "hello");
    }

    #[test]
    fn endpoint_appends_v1_chat_when_base_is_root() {
        let llm = HttpOpenAiLlm::new(cfg("https://api.example.com", "m", ""));
        assert_eq!(
            llm.endpoint().unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_appends_chat_when_base_ends_in_v1() {
        let llm = HttpOpenAiLlm::new(cfg("https://api.example.com/v1", "m", ""));
        assert_eq!(
            llm.endpoint().unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_passes_through_full_url() {
        let llm = HttpOpenAiLlm::new(cfg("https://api.example.com/v1/chat/completions", "m", ""));
        assert_eq!(
            llm.endpoint().unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        let llm = HttpOpenAiLlm::new(cfg("http://localhost:1234/v1/", "m", ""));
        assert_eq!(
            llm.endpoint().unwrap(),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_errors_when_base_empty() {
        let llm = HttpOpenAiLlm::new(cfg("", "m", ""));
        assert!(matches!(llm.endpoint(), Err(LlmError::NotReady(_))));
    }

    #[test]
    fn auth_header_is_none_when_key_empty() {
        let llm = HttpOpenAiLlm::new(cfg("http://x", "m", ""));
        assert!(llm.auth_header().is_none());
    }

    #[test]
    fn auth_header_prefixes_bearer_when_set() {
        let llm = HttpOpenAiLlm::new(cfg("http://x", "m", "sk-test"));
        assert_eq!(llm.auth_header().as_deref(), Some("Bearer sk-test"));
    }

    #[test]
    fn auth_header_trims_pasted_whitespace() {
        // Terminal copy/paste tends to capture trailing newlines.
        let llm = HttpOpenAiLlm::new(cfg("http://x", "m", "  sk-test\n"));
        assert_eq!(llm.auth_header().as_deref(), Some("Bearer sk-test"));
    }

    #[test]
    fn auth_header_treats_whitespace_only_as_empty() {
        let llm = HttpOpenAiLlm::new(cfg("http://x", "m", "   \n"));
        assert!(llm.auth_header().is_none());
    }

    #[test]
    fn build_messages_translates_roles() {
        let req = LlmRequest::new(vec![
            Turn::user("hi"),
            Turn::model("hello"),
            Turn::user("how are you"),
        ])
        .with_system("you are helpful");
        let msgs = HttpOpenAiLlm::build_messages(&req, false);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "you are helpful");
        assert_eq!(msgs[1].role, "user");
        // Trait-side "model" maps to wire-side "assistant".
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[3].role, "user");
    }

    #[test]
    fn build_messages_skips_empty_system() {
        let req = LlmRequest::new(vec![Turn::user("hi")]);
        let msgs = HttpOpenAiLlm::build_messages(&req, false);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn chat_template_kwargs_disables_thinking_for_llama_server() {
        // Qwen3 ignores `/no_think` in the system prompt but honours
        // `chat_template_kwargs.enable_thinking=false` from the
        // llama-server template renderer. The adapter sets it for the
        // llama-server kind so the streaming content sink isn't empty.
        let mut c = cfg("http://x", "qwen", "");
        c.kind = "llama-server".to_string();
        let llm = HttpOpenAiLlm::new(c);
        let kwargs = llm.chat_template_kwargs();
        assert_eq!(kwargs["enable_thinking"], serde_json::json!(false));
    }

    #[test]
    fn chat_template_kwargs_null_for_remote_openai() {
        // Remote OpenAI doesn't speak the kwarg; the field stays null
        // so it gets dropped from the wire payload via
        // skip_serializing_if.
        let llm = HttpOpenAiLlm::new(cfg("http://x", "gpt-4o-mini", ""));
        assert!(llm.chat_template_kwargs().is_null());
    }

    #[test]
    fn require_model_errors_on_empty() {
        let llm = HttpOpenAiLlm::new(cfg("http://x", "", ""));
        assert!(matches!(llm.require_model(), Err(LlmError::NotReady(_))));
    }
}
