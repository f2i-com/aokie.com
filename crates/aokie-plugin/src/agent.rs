//! In-plugin real-time LLM reply for the voice receptionist (voice feature).
//!
//! Streams a chat completion from the desktop's local LLM (llama.cpp / ollama)
//! and surfaces each finished SENTENCE as it's generated, so the plugin can
//! start speaking the reply sentence-by-sentence — far lower perceived latency
//! than waiting for the whole answer and a flow round-trip. Uses reqwest's
//! blocking API read line-by-line, which streams Server-Sent-Events without a
//! tokio runtime.

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Cloneable so a reply can run on a detached worker thread (AOK-CTRL-001):
/// the radio loop hands a clone to the worker and stays free to service
/// hangup/reject while the stream runs. reqwest clients are Arc inside, so a
/// clone shares the pinned, hardened connection pool.
#[derive(Clone)]
pub struct LlmClient {
    /// Err(reason) when the endpoint failed hardening (AOK-ENDPOINT-001) — every request
    /// then fails with that reason instead of silently using an unvalidated client.
    client: Result<reqwest::blocking::Client, String>,
    endpoint: String,
    model: Option<String>,
}

impl LlmClient {
    /// Build a client for `endpoint` (a `/v1/chat/completions` URL). Discovers
    /// the served model from `/v1/models` when `model` is None (so it reuses
    /// whatever the desktop has loaded, e.g. Qwen on llama.cpp).
    pub fn new(endpoint: String, model: Option<String>) -> Self {
        // Hardened client (audit AOK-ENDPOINT-001): redirects disabled, hostname endpoints
        // DNS-validated + pinned. A rejected endpoint yields a client that fails every
        // request with the rejection reason — caller transcripts never reach it.
        // Timeouts unchanged (audit AK-003: an unreachable endpoint must fail in seconds,
        // not hold the radio loop — call controls are blocked while a reply is in flight).
        let client = crate::endpoint_http::client_for(
            &endpoint,
            Duration::from_secs(30),
            Some(Duration::from_secs(3)),
        );
        if let Err(reason) = &client {
            eprintln!("[aokie-plugin] LLM endpoint rejected: {reason}");
        }
        let model = model.filter(|m| !m.trim().is_empty()).or_else(|| {
            client
                .as_ref()
                .ok()
                .and_then(|c| discover_model(c, &endpoint))
        });
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
    ///
    /// AOK-CTRL-001 cancellation contract: `cancel` is checked on EVERY SSE
    /// line — a punctuation-free stream (which never yields a sentence, so
    /// never invokes `on_sentence`) still aborts within one delta of the flag
    /// being set. `on_activity` fires on every line read from the socket; the
    /// caller's watchdog uses it as the liveness signal for the per-read idle
    /// deadline (reqwest 0.11 has no per-read timeout — only the whole-request
    /// deadline set in [`LlmClient::new`], which remains the hard backstop for
    /// a worker abandoned mid-read).
    pub fn stream_reply(
        &self,
        messages: serde_json::Value,
        cancel: &AtomicBool,
        mut on_activity: impl FnMut(),
        mut on_sentence: impl FnMut(&str) -> bool,
    ) -> Result<String, String> {
        let mut body = serde_json::json!({
            "messages": messages,
            "stream": true,
            "max_tokens": 120,
            // 0.35 (was 0.5): live calls showed real booking dates getting
            // scrambled on read-back ("Sunday, July 12th at 10 AM" for a
            // Sunday-July-19-6PM record) — factual precision beats sparkle
            // on a phone line.
            "temperature": 0.35,
            // Qwen3-class reasoning models otherwise burn the budget on a hidden
            // <think> block; ignored by models without a thinking mode.
            "chat_template_kwargs": { "enable_thinking": false },
        });
        if let Some(m) = &self.model {
            body["model"] = serde_json::json!(m);
        }
        let client = self
            .client
            .as_ref()
            .map_err(|reason| format!("llm endpoint rejected: {reason}"))?;
        let resp = client
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
        // True once ANY chunk was handed to synthesis (gates the eager
        // first-clause flush below to the reply's very first words).
        let mut flushed_any = false;
        // Failure observability (audit AOK-LLM-001): when the stream dies,
        // the error names WHICH phase — never produced a first token, or
        // stalled mid-reply — instead of a bare read error. Malformed chunks
        // are counted and, past a budget, abort instead of looping silently.
        let started = std::time::Instant::now();
        let mut first_delta_at: Option<std::time::Instant> = None;
        let mut last_delta_at = std::time::Instant::now();
        let mut deltas: u32 = 0;
        let mut malformed: u32 = 0;
        for line in reader.lines() {
            let line = line.map_err(|e| {
                match first_delta_at {
                    None => format!(
                        "llm produced NO first token within {:?} (endpoint up but not generating): {e}",
                        started.elapsed()
                    ),
                    Some(_) => format!(
                        "llm stream STALLED after {deltas} delta(s), {:?} since the last one: {e}",
                        last_delta_at.elapsed()
                    ),
                }
            })?;
            on_activity();
            // Per-line cancellation (AOK-CTRL-001): the caller hung up /
            // rejected / timed the reply out — stop pulling the stream now,
            // even if no sentence boundary ever arrives.
            if cancel.load(Ordering::Relaxed) {
                aborted = true;
                break;
            }
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            if data == "[DONE]" {
                break;
            }
            let v: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => {
                    malformed += 1;
                    if malformed > 20 {
                        return Err(format!(
                            "llm stream is emitting garbage ({malformed} malformed SSE chunks) — aborting"
                        ));
                    }
                    continue;
                }
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
            if first_delta_at.is_none() {
                first_delta_at = Some(std::time::Instant::now());
                eprintln!(
                    "[aokie-plugin] llm first token after {:?}",
                    started.elapsed()
                );
            }
            deltas += 1;
            last_delta_at = std::time::Instant::now();
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
                flushed_any = true;
            }
            if aborted {
                break;
            }
            // Eager FIRST clause (round 4): nothing has been spoken yet and
            // the model opened with a long sentence — flush at the first
            // clause break so the caller hears the reply start ~a clause
            // earlier. Only ever the first chunk; sentences rule after that.
            if !flushed_any {
                if let Some(idx) = first_clause_end(&buf) {
                    let done: String = buf.drain(..=idx).collect();
                    let done = done.trim();
                    if !done.is_empty() {
                        if !on_sentence(done) {
                            aborted = true;
                            break;
                        }
                        flushed_any = true;
                    }
                }
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

/// Byte index of the first CLAUSE break (comma/semicolon/colon followed by
/// whitespace) at least `MIN` chars in — used to start speaking a long
/// opening sentence a clause early. The next-char-arrived rule keeps "1,250"
/// and "9:30" intact (streaming-safe, same discipline as [`sentence_end`]).
fn first_clause_end(s: &str) -> Option<usize> {
    const MIN: usize = 24;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if i < MIN {
            continue;
        }
        if matches!(c, ',' | ';' | ':')
            && it.peek().is_some_and(|&(_, next)| next.is_whitespace())
        {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::sentence_end;

    /// Split `text` the way stream_reply does, feeding `delta`-sized pieces —
    /// returns the spoken chunks including the eager first clause and the
    /// trailing flush.
    fn chunks(text: &str, delta: usize) -> Vec<String> {
        let mut buf = String::new();
        let mut out = Vec::new();
        let mut flushed_any = false;
        let bytes: Vec<char> = text.chars().collect();
        for piece in bytes.chunks(delta) {
            buf.extend(piece);
            while let Some(idx) = sentence_end(&buf) {
                let done: String = buf.drain(..=idx).collect();
                let done = done.trim();
                if !done.is_empty() {
                    out.push(done.to_string());
                }
                flushed_any = true;
            }
            if !flushed_any {
                if let Some(idx) = super::first_clause_end(&buf) {
                    let done: String = buf.drain(..=idx).collect();
                    let done = done.trim();
                    if !done.is_empty() {
                        out.push(done.to_string());
                        flushed_any = true;
                    }
                }
            }
        }
        let rest = buf.trim();
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
        out
    }

    /// Round 4: a long OPENING sentence starts speaking at its first clause
    /// break; later sentences stay whole, and commas inside numbers or short
    /// openers never split.
    #[test]
    fn eager_first_clause_starts_speech_early() {
        assert_eq!(
            chunks("I can certainly book that table for ye, right after I check the tides.", 4),
            vec![
                "I can certainly book that table for ye,",
                "right after I check the tides.",
            ]
        );
        // Only the FIRST chunk is eager — the second sentence keeps its commas.
        assert_eq!(
            chunks("Aye that works for me matey, good choice. We open at nine, ten on Sundays.", 5),
            vec![
                "Aye that works for me matey,",
                "good choice.",
                "We open at nine, ten on Sundays.",
            ]
        );
        // Short openers ("Hi Lance, ...") never split at the comma.
        assert_eq!(
            chunks("Hi Lance, welcome back to the diner today!", 3),
            vec!["Hi Lance, welcome back to the diner today!"]
        );
        // Digits around ':' / ',' stay intact.
        assert_eq!(
            chunks("The total for the party comes to 1,250 doubloons exactly.", 4),
            vec!["The total for the party comes to 1,250 doubloons exactly."]
        );
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
    // Deterministic + explainable (audit AOK-LLM-001): the served list's
    // order is not guaranteed stable, so sort ids and take the first —
    // the same endpoint always yields the same choice, and the log says
    // what was available.
    let mut ids: Vec<String> = v
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .collect();
    ids.sort();
    let chosen = ids.first().cloned();
    if let Some(c) = &chosen {
        eprintln!(
            "[aokie-plugin] llm model auto-selected '{c}' (no aiModel setting; served: [{}])",
            ids.join(", ")
        );
    }
    chosen
}

/// Find a reachable local LLM: return the first candidate whose `/v1/models`
/// answers. `configured` (the `aiEndpoint` setting) wins; else probe llama.cpp
/// (:8080) then ollama (:11434) — reusing whatever the desktop has running.
pub fn discover_endpoint(configured: Option<&str>) -> Option<String> {
    let candidates: Vec<String> = match configured {
        Some(e) if !e.trim().is_empty() => vec![e.trim().to_string()],
        _ => vec![
            "http://127.0.0.1:8080/v1/chat/completions".to_string(),
            "http://127.0.0.1:11434/v1/chat/completions".to_string(),
        ],
    };
    for ep in candidates {
        // Hardened per-endpoint client (AOK-ENDPOINT-001): a candidate that fails
        // validation (bad URL / non-public DNS) is skipped, never probed.
        let Ok(client) = crate::endpoint_http::client_for(&ep, Duration::from_secs(3), None) else {
            continue;
        };
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
