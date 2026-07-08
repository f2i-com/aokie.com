//! In-plugin real-time LLM reply for the voice receptionist (voice feature).
//!
//! Streams a chat completion from the desktop's local LLM (llama.cpp / ollama)
//! and surfaces each finished SENTENCE as it's generated, so the plugin can
//! start speaking the reply sentence-by-sentence — far lower perceived latency
//! than waiting for the whole answer and a flow round-trip. Uses reqwest's
//! blocking API read line-by-line, which streams Server-Sent-Events without a
//! tokio runtime.

use std::io::{BufRead, BufReader};
use std::time::Duration;

pub struct LlmClient {
    client: reqwest::blocking::Client,
    endpoint: String,
    model: Option<String>,
}

impl LlmClient {
    /// Build a client for `endpoint` (a `/v1/chat/completions` URL). Discovers
    /// the served model from `/v1/models` when `model` is None (so it reuses
    /// whatever the desktop has loaded, e.g. Qwen on llama.cpp).
    pub fn new(endpoint: String, model: Option<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        let model = model
            .filter(|m| !m.trim().is_empty())
            .or_else(|| discover_model(&client, &endpoint));
        Self { client, endpoint, model }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Stream a reply for `messages` (an OpenAI chat array). Calls `on_sentence`
    /// with each complete sentence as it's produced; return `false` from it to
    /// abort (barge-in / hangup). Returns the full reply text.
    pub fn stream_reply(
        &self,
        messages: serde_json::Value,
        mut on_sentence: impl FnMut(&str) -> bool,
    ) -> Result<String, String> {
        let mut body = serde_json::json!({
            "messages": messages,
            "stream": true,
            "max_tokens": 120,
            "temperature": 0.5,
            // Qwen3-class reasoning models otherwise burn the budget on a hidden
            // <think> block; ignored by models without a thinking mode.
            "chat_template_kwargs": { "enable_thinking": false },
        });
        if let Some(m) = &self.model {
            body["model"] = serde_json::json!(m);
        }
        let resp = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .map_err(|e| format!("llm request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("llm responded {}", resp.status()));
        }

        let reader = BufReader::new(resp);
        let mut full = String::new();
        let mut buf = String::new();
        let mut aborted = false;
        for line in reader.lines() {
            let line = line.map_err(|e| format!("llm stream read: {e}"))?;
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            if data == "[DONE]" {
                break;
            }
            let v: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let delta = v
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if delta.is_empty() {
                continue;
            }
            full.push_str(delta);
            buf.push_str(delta);
            // Flush every complete sentence so TTS starts on the first one.
            while let Some(idx) = sentence_end(&buf) {
                let done: String = buf.drain(..=idx).collect();
                let done = done.trim();
                if !done.is_empty() && !on_sentence(done) {
                    aborted = true;
                    break;
                }
            }
            if aborted {
                break;
            }
        }
        // Speak any trailing partial sentence.
        let rest = buf.trim();
        if !aborted && !rest.is_empty() {
            on_sentence(rest);
        }
        Ok(full)
    }
}

/// Byte index of the first sentence-ending punctuation, at least a few chars in
/// so we don't chop a leading fragment before there's a phrase worth speaking.
fn sentence_end(s: &str) -> Option<usize> {
    const MIN: usize = 8;
    s.char_indices()
        .find(|&(i, c)| i >= MIN && matches!(c, '.' | '!' | '?' | '\n'))
        .map(|(i, c)| i + c.len_utf8() - 1)
}

/// First model id advertised at `<endpoint>/../models`.
fn discover_model(client: &reqwest::blocking::Client, chat_endpoint: &str) -> Option<String> {
    let url = chat_endpoint.replace("/chat/completions", "/models");
    let resp = client.get(&url).timeout(Duration::from_secs(4)).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().ok()?;
    v.get("data")?
        .as_array()?
        .iter()
        .find_map(|m| m.get("id").and_then(|i| i.as_str()))
        .map(|s| s.to_string())
}

/// Find a reachable local LLM: return the first candidate whose `/v1/models`
/// answers. `configured` (the `aiEndpoint` setting) wins; else probe llama.cpp
/// (:8080) then ollama (:11434) — reusing whatever the desktop has running.
pub fn discover_endpoint(configured: Option<&str>) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let candidates: Vec<String> = match configured {
        Some(e) if !e.trim().is_empty() => vec![e.trim().to_string()],
        _ => vec![
            "http://127.0.0.1:8080/v1/chat/completions".to_string(),
            "http://127.0.0.1:11434/v1/chat/completions".to_string(),
        ],
    };
    for ep in candidates {
        let models = ep.replace("/chat/completions", "/models");
        if client
            .get(&models)
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return Some(ep);
        }
    }
    None
}
