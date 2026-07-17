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

/// Shrink a caller-turn PCM clip before it rides an LLM request (2026-07-17
/// latency round): trim leading/trailing silence and collapse long internal
/// pauses, so the attached WAV — and the audio tokens the model must prefill —
/// only cover actual speech. Conservative by design: the silence threshold sits
/// well below the STT speech gate (RMS 350), a padded pre/post-roll is kept
/// around every speech region, and anything that would trim the clip to under
/// 200 ms returns the input unchanged.
pub fn trim_silence_for_llm(pcm: &[i16], sample_rate: u32) -> Vec<i16> {
    const SILENCE_RMS: f64 = 150.0;
    let win = (sample_rate as usize / 50).max(1); // 20 ms windows
    let pad_windows = 6; // 120 ms kept around each speech region
    let collapse_after = 30; // every internal pause is capped at 600 ms
    let min_out = (sample_rate as usize) / 5; // 200 ms safety floor

    if pcm.len() < win * 4 {
        return pcm.to_vec();
    }
    let loud: Vec<bool> = pcm
        .chunks(win)
        .map(|w| {
            let sum: f64 = w.iter().map(|&s| (s as f64) * (s as f64)).sum();
            (sum / w.len() as f64).sqrt() > SILENCE_RMS
        })
        .collect();
    let Some(first) = loud.iter().position(|&l| l) else {
        // No speech at all — send a short slice so the model still hears
        // "silence" rather than nothing (the prompt handles that case).
        return pcm[..pcm.len().min(win * 25)].to_vec();
    };
    let last = loud.iter().rposition(|&l| l).unwrap_or(first);

    let start = first.saturating_sub(pad_windows);
    let end = (last + 1 + pad_windows).min(loud.len());
    let mut out: Vec<i16> = Vec::with_capacity((end - start) * win);
    let mut silent_run = 0usize;
    for (idx, is_loud) in loud.iter().enumerate().take(end).skip(start) {
        if *is_loud {
            silent_run = 0;
        } else {
            silent_run += 1;
            if silent_run > collapse_after {
                continue; // pause budget spent — drop the rest of this gap
            }
        }
        let s = idx * win;
        let e = (s + win).min(pcm.len());
        out.extend_from_slice(&pcm[s..e]);
    }
    if out.len() < min_out {
        return pcm.to_vec();
    }
    out
}

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

    /// A clone of this client that names a different model on its requests —
    /// the transcript-correction lane can use a lighter audio model on the
    /// SAME server without a second connection pool.
    pub fn with_model(&self, model: String) -> Self {
        let mut c = self.clone();
        c.model = Some(model);
        c
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
    /// Ring-time KV pre-warm (2026-07-14): process the reply request's
    /// PREFIX (system prompt + history) with a 1-token generation so
    /// llama.cpp's prompt cache is hot when the real reply arrives — its
    /// first token then only pays for the caller's new words. Cheap on the
    /// serving GPU; fire-and-forget from a worker thread, NEVER the radio
    /// loop (an on-loop HTTP call delayed the ANSWER once already).
    pub fn warm_prefix(&self, messages: serde_json::Value) -> Result<(), String> {
        let mut body = serde_json::json!({
            "messages": messages,
            "stream": false,
            "max_tokens": 1,
            "temperature": 0.0,
            "cache_prompt": true,
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
            .map_err(|e| format!("llm warm failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("llm warm responded {}", resp.status()));
        }
        Ok(())
    }

    /// audioTranscript: ask the audio-capable model to CORRECT the
    /// on-device STT from the turn's actual audio. Non-streaming, one small
    /// generation, called from a DETACHED worker only — never the radio
    /// loop (the reply path must not wait on this). The prompt frames it as
    /// a correction (the STT draft rides along as a hint) so the output
    /// stays close to the recognizer when it was already right instead of
    /// re-imagining the sentence. `context` carries the last few
    /// conversation turns (text only) so ambiguous audio resolves toward
    /// the dialogue's domain — live call e150a269: the caller asked about
    /// "ointments" at a DENTAL receptionist and the context-free model kept
    /// it verbatim instead of hearing "appointments".
    pub fn transcribe_turn(
        &self,
        wav_b64: &str,
        stt_text: &str,
        context: &str,
        prev_draft: Option<&str>,
    ) -> Result<String, String> {
        let mut hint = String::new();
        if !context.is_empty() {
            hint.push_str(&format!("Conversation so far:\n{context}\n\n"));
        }
        if let Some(p) = prev_draft {
            // Split-utterance continuity: the WAV starts with the caller's
            // previous utterance so the model hears the sentence flow — it
            // must still output only the FINAL utterance.
            hint.push_str(&format!(
                "The audio STARTS with the caller's previous utterance (draft: {p}) for continuity - transcribe ONLY the final utterance after it.\n\n"
            ));
        }
        hint.push_str(&format!("Recognizer draft: {stt_text}"));
        let mut body = serde_json::json!({
            "messages": [
                { "role": "system", "content": "You transcribe one short phone-call utterance from its audio. A speech recognizer's draft and the recent conversation are provided; correct any words the recognizer got wrong using the audio, and use the conversation only to resolve unclear or ambiguous words toward what the caller plainly meant to say. Never import words from the conversation that the audio does not support. When the draft already reads as fluent natural speech, prefer it - change only words the audio clearly contradicts. If the audio is silence or a filler sound, output the draft unchanged. Reply with ONLY the corrected transcription - the caller's exact words, no quotes, no commentary." },
                { "role": "user", "content": [
                    { "type": "input_audio", "input_audio": { "data": wav_b64, "format": "wav" } },
                    { "type": "text", "text": hint },
                ] },
            ],
            "stream": false,
            "max_tokens": 200,
            "temperature": 0.0,
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
            .map_err(|e| format!("transcript correction failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("transcript correction responded {}", resp.status()));
        }
        let v: serde_json::Value = resp
            .json()
            .map_err(|e| format!("transcript correction body unreadable: {e}"))?;
        let content = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(content)
    }

    /// Encode 16-bit mono PCM as a base64 WAV — the `input_audio` content
    /// part for audio-capable models (Gemma 3n / Qwen2-Audio class) served
    /// by llama-server's OpenAI-compatible endpoint. Used only when the
    /// `sendAudio` setting is on.
    pub fn wav_base64(pcm: &[i16], sample_rate: u32) -> String {
        use base64::Engine as _;
        let data_len = (pcm.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + pcm.len() * 2);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for s in pcm {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        base64::engine::general_purpose::STANDARD.encode(&wav)
    }

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
            // Anti-template-spiral (live call 2c00cac0: Gemma 4 answered three
            // turns in a row with the identical deferral sentence, then
            // degenerated to an EMPTY reply). llama.cpp-native; other
            // providers ignore unknown fields.
            "repeat_penalty": 1.15,
            // llama.cpp prompt/KV caching: with the ring-time prefix warm the
            // real reply's first token only pays for the caller's NEW words.
            // Providers without the extension ignore the field.
            "cache_prompt": true,
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
            let line = line.map_err(|e| match first_delta_at {
                None => format!(
                    "llm produced NO first token within {:?} (endpoint up but not generating): {e}",
                    started.elapsed()
                ),
                Some(_) => format!(
                    "llm stream STALLED after {deltas} delta(s), {:?} since the last one: {e}",
                    last_delta_at.elapsed()
                ),
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
        if matches!(c, ',' | ';' | ':') && it.peek().is_some_and(|&(_, next)| next.is_whitespace())
        {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::sentence_end;
    use super::trim_silence_for_llm;

    /// 1 s of silence + 1 s of tone + 2 s of silence + 1 s of tone + 1 s of
    /// silence: the trim must drop the outer silence (minus pad), cap the
    /// internal pause, and keep every speech window.
    #[test]
    fn trim_silence_drops_padding_and_caps_pauses() {
        const SR: u32 = 16_000;
        let sec = SR as usize;
        let tone = |n: usize| -> Vec<i16> {
            (0..n)
                .map(|i| ((i as f64 * 0.3).sin() * 3000.0) as i16)
                .collect()
        };
        let mut pcm = vec![0i16; sec];
        pcm.extend(tone(sec));
        pcm.extend(vec![0i16; 2 * sec]);
        pcm.extend(tone(sec));
        pcm.extend(vec![0i16; sec]);
        let out = trim_silence_for_llm(&pcm, SR);
        // Expected ≈ 120ms pad + 1s tone + 600ms capped pause + 1s tone +
        // 120ms pad ≈ 2.85 s, from 6 s in. Assert generous bounds.
        assert!(out.len() < 3 * sec + sec / 2, "too little trimmed: {}", out.len());
        assert!(out.len() > 2 * sec, "speech lost: {}", out.len());
    }

    /// Quiet-only audio still returns a short non-empty clip (the correction
    /// prompt handles "silence" explicitly — sending nothing would error).
    #[test]
    fn trim_silence_on_pure_silence_returns_short_clip() {
        const SR: u32 = 16_000;
        let pcm = vec![0i16; 5 * SR as usize];
        let out = trim_silence_for_llm(&pcm, SR);
        assert!(!out.is_empty());
        assert!(out.len() <= SR as usize / 2);
    }

    /// Continuous speech passes through untouched; tiny clips are never
    /// trimmed below the safety floor.
    #[test]
    fn trim_silence_keeps_continuous_speech_and_tiny_clips() {
        const SR: u32 = 16_000;
        let tone: Vec<i16> = (0..2 * SR as usize)
            .map(|i| ((i as f64 * 0.3).sin() * 3000.0) as i16)
            .collect();
        let out = trim_silence_for_llm(&tone, SR);
        assert!(out.len() >= tone.len() - SR as usize / 4);
        let tiny = vec![100i16; 500];
        assert_eq!(trim_silence_for_llm(&tiny, SR), tiny);
    }

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
            chunks(
                "I can certainly book that table for ye, right after I check the tides.",
                4
            ),
            vec![
                "I can certainly book that table for ye,",
                "right after I check the tides.",
            ]
        );
        // Only the FIRST chunk is eager — the second sentence keeps its commas.
        assert_eq!(
            chunks(
                "Aye that works for me matey, good choice. We open at nine, ten on Sundays.",
                5
            ),
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
            chunks(
                "The total for the party comes to 1,250 doubloons exactly.",
                4
            ),
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
        assert_eq!(
            chunks("First line\nSecond line", 100),
            vec!["First line", "Second line"]
        );
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
