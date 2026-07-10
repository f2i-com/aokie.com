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
            // Audit AK-003: an unreachable endpoint must fail in seconds, not
            // hold the radio loop for the full request timeout — call controls
            // are blocked while a reply request is in flight.
            .connect_timeout(Duration::from_secs(3))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        let model = model
            .filter(|m| !m.trim().is_empty())
            .or_else(|| discover_model(&client, &endpoint));
        Self {
            client,
            endpoint,
            model,
        }
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
///
/// STREAMING-SAFE (the "Mesure" live-call bug): `.`/`!`/`?` end a sentence only
/// when the character AFTER them has already arrived and is whitespace. That
/// keeps the dots inside "9 A.M.", decimals ("5.50"), "e.g." and web addresses
/// from splitting a chunk mid-token — "…at 9 A." + "M. Sure…" was synthesized
/// as "Mesure" — and it refuses to split on a period that is merely the last
/// character received so far (the next delta may continue the token; the
/// caller's trailing-flush speaks whatever remains at stream end). `\n` is
/// always a boundary. The per-chunk speech normalizer can only rewrite
/// "a.m."/"AM" it can SEE, so chunks must never end mid-abbreviation.
fn sentence_end(s: &str) -> Option<usize> {
    const MIN: usize = 8;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if i < MIN {
            continue;
        }
        match c {
            '\n' => return Some(i),
            '.' | '!' | '?' => {
                if it.peek().is_some_and(|&(_, next)| next.is_whitespace()) {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::sentence_end;

    /// Split `text` the way stream_reply does, feeding `delta`-sized pieces —
    /// returns the spoken chunks including the trailing flush.
    fn chunks(text: &str, delta: usize) -> Vec<String> {
        let mut buf = String::new();
        let mut out = Vec::new();
        let bytes: Vec<char> = text.chars().collect();
        for piece in bytes.chunks(delta) {
            buf.extend(piece);
            while let Some(idx) = sentence_end(&buf) {
                let done: String = buf.drain(..=idx).collect();
                let done = done.trim();
                if !done.is_empty() {
                    out.push(done.to_string());
                }
            }
        }
        let rest = buf.trim();
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
        out
    }

    /// The live-call bug: "…9 A.M. …" must never split between "A." and "M."
    /// (pocket-tts voiced the "M. Sure" chunk as "Mesure"), for EVERY possible
    /// stream-delta alignment.
    #[test]
    fn never_splits_inside_a_dotted_meridiem() {
        let reply = "Sure — I have 9:30 A.M. available. Anything else?";
        for delta in 1..=reply.chars().count() {
            let got = chunks(reply, delta);
            assert_eq!(
                got,
                vec![
                    "Sure — I have 9:30 A.M.".to_string(),
                    "available.".to_string(),
                    "Anything else?".to_string(),
                ],
                "delta {delta} split mid-abbreviation: {got:?}"
            );
        }
    }

    #[test]
    fn decimals_and_dotted_abbreviations_do_not_split() {
        assert_eq!(
            chunks("That will be 5.50 in total. See you then.", 3),
            vec!["That will be 5.50 in total.", "See you then."]
        );
        assert_eq!(
            chunks("We open at 10 a.m. and close at 9 p.m. every day.", 5),
            vec!["We open at 10 a.m.", "and close at 9 p.m.", "every day."]
        );
        assert_eq!(
            chunks("Our site is example.com if you need it.", 4),
            vec!["Our site is example.com if you need it."]
        );
    }

    #[test]
    fn ordinary_boundaries_still_split_promptly() {
        assert_eq!(
            chunks("Hello there. How can I help you today?", 100),
            vec!["Hello there.", "How can I help you today?"]
        );
        // Newlines are always boundaries.
        assert_eq!(chunks("First line\nSecond line", 100), vec!["First line", "Second line"]);
        // A period as the LAST char seen so far waits for the next delta /
        // the trailing flush instead of splitting blind.
        assert_eq!(sentence_end("Booked for 9 A."), None);
        assert_eq!(chunks("Booked for 10 a.m.", 1), vec!["Booked for 10 a.m."]);
    }
}

/// First model id advertised at `<endpoint>/../models`.
fn discover_model(client: &reqwest::blocking::Client, chat_endpoint: &str) -> Option<String> {
    let url = chat_endpoint.replace("/chat/completions", "/models");
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(4))
        .send()
        .ok()?;
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
