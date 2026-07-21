//! HTTP STT/TTS lanes: OpenAI-spec endpoints, streaming PCM chunker, fallbacks.

#[allow(unused_imports)]
use super::*;

#[cfg(feature = "voice")]
pub(super) enum SttWork {
    /// One finished caller utterance, stamped with the call it belongs to
    /// (audit C-05): the worker skips jobs whose generation is no longer
    /// current, and the consumer drops results the same way — a slow
    /// transcription from call A can never be attributed to call B.
    Utterance {
        generation: u64,
        utterance: u32,
        samples: Vec<f32>,
    },
    /// Priority control probe (phase 2 + §3.2): a SNAPSHOT of the caller's
    /// overlap speech while Aokie is talking, transcribed so a spoken
    /// "wait"/"stop" can cut playback mid-sentence instead of after the
    /// reply. Local-engine ONLY (a probe must never widen audio egress to an
    /// HTTP endpoint), answered on the dedicated probe channel, and NEVER
    /// consuming/truncating the content buffer — the full utterance still
    /// arrives as a normal `Utterance` at its endpoint.
    Probe {
        generation: u64,
        /// Which probe consumer this snapshot belongs to (066d2237 stale-probe
        /// fencing): results echo it back in `SttResult::utterance`, and each
        /// consumer drops results carrying a foreign lane id. Sherpa-fast
        /// replies exposed the race — a probe covering the caller's ALREADY-
        /// ANSWERED previous turn landed after the next reply's lane armed and
        /// was credited as live overlap, cutting the very reply answering it.
        /// Lane 0 = the main loop's live-hypothesis lane; playback
        /// [`SttProbeLane`]s allocate unique ids from [`PROBE_LANE_SEQ`].
        lane: u32,
        samples: Vec<f32>,
    },
    Configure {
        endpoint: Option<String>,
    },
    ResetCall,
    /// Ring-time pre-warm (2026-07-14): load the local STT engine while the
    /// phone is still ringing, so the caller's first words transcribe hot.
    Warm,
}

/// A finished transcription, still carrying the identity of the call whose
/// audio produced it.
#[cfg(feature = "voice")]
pub(super) struct SttResult {
    pub(super) generation: u64,
    pub(super) utterance: u32,
    pub(super) text: String,
}

/// The HTTP-TTS endpoint with its sticky per-call fallback. Owned by the
/// synth WORKER since phase 2 (crate::synth) — the radio thread only forwards
/// configure/reset messages.
#[cfg(feature = "voice")]
pub(crate) struct HttpTtsRuntime {
    pub(super) fallback: HttpSpeechFallback,
    /// VOX-404: the endpoint answered a streaming-PCM attempt with a
    /// non-streaming response this call (non-200 / no `X-Sample-Rate`) —
    /// spec-compliant third parties do exactly that, so don't burn an extra
    /// request per span re-asking; go straight to the whole-utterance wav
    /// path until the next call / reconfigure.
    pub(super) pcm_stream_unsupported: bool,
}

#[cfg(feature = "voice")]
impl HttpTtsRuntime {
    pub(crate) fn from_env(var: &str) -> Self {
        Self {
            fallback: HttpSpeechFallback::from_env(var),
            pcm_stream_unsupported: false,
        }
    }

    pub(crate) fn configure(&mut self, endpoint: Option<String>) {
        self.fallback.configure(endpoint);
        self.pcm_stream_unsupported = false;
    }

    pub(crate) fn reset_call(&mut self) {
        self.fallback.reset_call();
        self.pcm_stream_unsupported = false;
    }

    /// Whether a span should attempt the streaming-PCM request first.
    pub(crate) fn pcm_streaming_enabled_for_call(&self) -> bool {
        !self.pcm_stream_unsupported
    }

    /// Remember (per call) that the endpoint doesn't stream PCM.
    pub(crate) fn mark_pcm_stream_unsupported(&mut self) {
        self.pcm_stream_unsupported = true;
    }

    pub(crate) fn endpoint_for_call(&self) -> Option<String> {
        self.fallback.endpoint_for_call().map(str::to_string)
    }

    pub(crate) fn mark_failed_for_call(&mut self) -> bool {
        self.fallback.mark_failed_for_call()
    }
}

/// One whole-utterance HTTP TTS request against a hardened per-endpoint
/// client (AOK-ENDPOINT-001). Worker-side entry point (crate::synth).
#[cfg(feature = "voice")]
pub(crate) fn http_tts_synthesize_at(
    endpoint: &str,
    text: &str,
    voice: &str,
) -> Result<crate::speech_wire::WavPcm, String> {
    let client =
        http_speech_client(endpoint).map_err(|e| format!("TTS endpoint rejected ({e})"))?;
    http_tts_synthesize(&client, endpoint, text, voice)
}

/// Hardened per-endpoint speech client (audit AOK-ENDPOINT-001): redirects disabled,
/// hostname endpoints DNS-validated + pinned, cached per endpoint. Err = the endpoint
/// must not receive caller audio; callers surface it via their existing failure paths
/// (mark_failed_for_call → sticky in-process fallback).
#[cfg(feature = "voice")]
pub(super) fn http_speech_client(endpoint: &str) -> Result<reqwest::blocking::Client, String> {
    crate::endpoint_http::client_for(endpoint, std::time::Duration::from_secs(30), None)
}

/// OpenAI-spec STT (VOX-403): `POST <endpoint>` as `multipart/form-data` with
/// a `file` part (audio.wav — 16 kHz mono PCM16 WAV) + `response_format=json`,
/// expecting `{"text": …}` back. This is the `/v1/audio/transcriptions` wire
/// shape, so ANY OpenAI-compatible STT server works; the legacy base64-JSON
/// body is gone (the aokie services accept multipart too).
#[cfg(feature = "voice")]
pub(super) fn http_stt_transcribe(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    samples_16k: &[f32],
) -> Result<String, String> {
    let pcm: Vec<i16> = samples_16k
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let wav = crate::speech_wire::encode_wav_pcm16_mono(&pcm, 16_000);
    let form = stt_multipart_form(wav).map_err(|e| format!("stt multipart build failed: {e}"))?;
    let resp = crate::endpoint_http::with_gateway_bearer(client.post(endpoint), endpoint)
        .multipart(form)
        .send()
        .map_err(|e| format!("stt http request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("stt http responded {}", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .map_err(|e| format!("stt http response was not JSON: {e}"))?;
    Ok(v.get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "stt http response missing text".to_string())?
        .trim()
        .to_string())
}

/// The OpenAI transcription multipart body: `file` (audio.wav, audio/wav) +
/// `response_format=json`. Extracted so the request shape stays testable.
#[cfg(feature = "voice")]
pub(super) fn stt_multipart_form(wav: Vec<u8>) -> Result<reqwest::blocking::multipart::Form, String> {
    let file = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    Ok(reqwest::blocking::multipart::Form::new()
        .part("file", file)
        .text("response_format", "json"))
}

/// Whole-utterance OpenAI-spec TTS: `POST` JSON `{input, voice,
/// response_format:"wav"}`, accepting raw audio bytes back (or, tolerated,
/// JSON `{b64_json|audio}` base64). The fallback rung under the streaming-PCM
/// attempt ([`http_tts_stream_pcm_at`]).
#[cfg(feature = "voice")]
pub(super) fn http_tts_synthesize(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    text: &str,
    voice: &str,
) -> Result<crate::speech_wire::WavPcm, String> {
    let resp = crate::endpoint_http::with_gateway_bearer(client.post(endpoint), endpoint)
        .json(&serde_json::json!({ "input": text, "voice": voice, "response_format": "wav" }))
        .send()
        .map_err(|e| format!("tts http request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("tts http responded {}", resp.status()));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bytes = resp
        .bytes()
        .map_err(|e| format!("tts http response read failed: {e}"))?
        .to_vec();
    let looks_json = bytes.iter().find(|b| !b.is_ascii_whitespace()).copied() == Some(b'{');
    let audio_bytes = if content_type.contains("json") || looks_json {
        decode_tts_json_audio(&bytes)?
    } else {
        bytes
    };
    let wav = crate::speech_wire::decode_wav_mono_i16(&audio_bytes)?;
    if wav.samples.is_empty() {
        return Err("tts http response contained no audio samples".to_string());
    }
    Ok(wav)
}

#[cfg(feature = "voice")]
pub(super) fn decode_tts_json_audio(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let v: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("tts http JSON response parse failed: {e}"))?;
    let audio = v
        .get("b64_json")
        .or_else(|| v.get("audio"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "tts http JSON response missing b64_json/audio".to_string())?;
    crate::speech_wire::base64_decode_text(audio)
}

/// Outcome of one streaming-PCM TTS attempt ([`http_tts_stream_pcm_at`]).
/// The variants encode the fallback ladder: `Unsupported` = nothing was
/// consumed, try the whole-utterance wav request; `FailedMidStream` = audio
/// already reached the caller's ear, the span must FAIL (re-synthesizing it
/// would double-speak the prefix); `Aborted` = the consumer stopped it
/// (epoch moved — dropping the connection is the cancel signal the server
/// sees); `Streamed` = the span completed.
#[cfg(feature = "voice")]
pub(crate) enum HttpTtsStream {
    Streamed,
    Aborted,
    Unsupported(String),
    FailedMidStream(String),
}

/// Incremental PCM16LE → target-rate block chunker (VOX-404). Feed raw body
/// bytes as they arrive; it carries the odd trailing byte across reads,
/// accumulates ~240 ms of SOURCE-rate samples, and emits blocks already
/// resampled to the SCO rate — the same block granularity the synth worker's
/// local streaming path ships. Pure (no IO) so the slicing/resample decision
/// is unit-testable.
#[cfg(feature = "voice")]
pub(crate) struct PcmStreamChunker {
    pub(super) src_rate: u32,
    pub(super) target_rate: u32,
    /// Source-rate samples not yet emitted.
    pub(super) pending: Vec<i16>,
    /// A dangling low byte when a read split an i16 sample.
    pub(super) carry: Option<u8>,
    /// Emit threshold in SOURCE-rate samples (~240 ms).
    pub(super) block: usize,
}

#[cfg(feature = "voice")]
impl PcmStreamChunker {
    pub(crate) fn new(src_rate: u32, target_rate: u32) -> Self {
        Self {
            src_rate,
            target_rate,
            pending: Vec::new(),
            carry: None,
            block: (src_rate as usize / 4).max(160),
        }
    }

    pub(super) fn resample(&self, samples: &[i16]) -> Vec<i16> {
        if self.src_rate == self.target_rate {
            samples.to_vec()
        } else {
            crate::speech_wire::resample_i16_mono(samples, self.src_rate, self.target_rate)
        }
    }

    /// Feed raw bytes; returns zero or more target-rate blocks ready to ship.
    pub(crate) fn push(&mut self, mut bytes: &[u8]) -> Vec<Vec<i16>> {
        if let Some(lo) = self.carry.take() {
            if let Some((&hi, rest)) = bytes.split_first() {
                self.pending.push(i16::from_le_bytes([lo, hi]));
                bytes = rest;
            } else {
                self.carry = Some(lo);
                return Vec::new();
            }
        }
        let mut iter = bytes.chunks_exact(2);
        for pair in &mut iter {
            self.pending.push(i16::from_le_bytes([pair[0], pair[1]]));
        }
        if let [lo] = iter.remainder() {
            self.carry = Some(*lo);
        }
        let mut out = Vec::new();
        while self.pending.len() >= self.block {
            let chunk: Vec<i16> = self.pending.drain(..self.block).collect();
            out.push(self.resample(&chunk));
        }
        out
    }

    /// Flush whatever remains at end-of-stream (a dangling odd byte is
    /// dropped — a truncated final sample, not audio).
    pub(crate) fn finish(&mut self) -> Option<Vec<i16>> {
        if self.pending.is_empty() {
            return None;
        }
        let tail: Vec<i16> = std::mem::take(&mut self.pending);
        Some(self.resample(&tail))
    }
}

/// Streaming-PCM TTS attempt (VOX-404): `POST` JSON `{input, voice,
/// response_format:"pcm"}`; a streaming server answers 200 with an
/// `X-Sample-Rate` header and raw PCM16LE mono in the body, which is read
/// INCREMENTALLY — each ~240 ms block is resampled to `target_rate` and
/// handed to `on_block` as it arrives, so pointing `ttsEndpoint` at a
/// service is no longer a whole-utterance latency hit. `on_block` returning
/// `false` (epoch moved) drops the connection, which is how the server
/// learns the span was cancelled. Non-200 / missing header = not a
/// streaming server → [`HttpTtsStream::Unsupported`], nothing consumed.
#[cfg(feature = "voice")]
pub(crate) fn http_tts_stream_pcm_at(
    endpoint: &str,
    text: &str,
    voice: &str,
    target_rate: u32,
    mut on_block: impl FnMut(Vec<i16>) -> bool,
) -> HttpTtsStream {
    use std::io::Read as _;
    let client = match http_speech_client(endpoint) {
        Ok(c) => c,
        Err(e) => return HttpTtsStream::Unsupported(format!("TTS endpoint rejected ({e})")),
    };
    let resp = match crate::endpoint_http::with_gateway_bearer(client.post(endpoint), endpoint)
        .json(&serde_json::json!({ "input": text, "voice": voice, "response_format": "pcm" }))
        .send()
    {
        Ok(r) => r,
        Err(e) => return HttpTtsStream::Unsupported(format!("tts stream request failed: {e}")),
    };
    if !resp.status().is_success() {
        return HttpTtsStream::Unsupported(format!("tts stream responded {}", resp.status()));
    }
    let Some(src_rate) = resp
        .headers()
        .get("x-sample-rate")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&r| (4_000..=192_000).contains(&r))
    else {
        return HttpTtsStream::Unsupported(
            "tts response has no usable X-Sample-Rate header (not a streaming-PCM server)"
                .to_string(),
        );
    };
    let mut chunker = PcmStreamChunker::new(src_rate, target_rate);
    let mut body = resp;
    let mut buf = [0u8; 8192];
    let mut shipped = false;
    loop {
        match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for block in chunker.push(&buf[..n]) {
                    if !on_block(block) {
                        return HttpTtsStream::Aborted;
                    }
                    shipped = true;
                }
            }
            Err(e) => {
                // Before any audio reached the pipeline the wav fallback can
                // still speak the whole span; after, the span must fail.
                return if shipped {
                    HttpTtsStream::FailedMidStream(format!("tts stream died mid-span: {e}"))
                } else {
                    HttpTtsStream::Unsupported(format!("tts stream died before audio: {e}"))
                };
            }
        }
    }
    if let Some(tail) = chunker.finish() {
        if !on_block(tail) {
            return HttpTtsStream::Aborted;
        }
        shipped = true;
    }
    if !shipped {
        return HttpTtsStream::Unsupported("tts stream contained no audio".to_string());
    }
    HttpTtsStream::Streamed
}

#[cfg(all(test, feature = "voice"))]
pub(super) mod pcm_stream_chunker_tests {
    use super::PcmStreamChunker;

    fn le_bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn same_rate_blocks_pass_through_at_240ms_granularity() {
        // 8 kHz → 8 kHz: block threshold = 2000 samples (~250 ms), no resample.
        let mut c = PcmStreamChunker::new(8_000, 8_000);
        let samples: Vec<i16> = (0..2_500).map(|i| i as i16).collect();
        let blocks = c.push(&le_bytes(&samples));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].len(), 2_000);
        assert_eq!(blocks[0][..4], [0, 1, 2, 3]);
        // The 500-sample remainder flushes at end-of-stream.
        let tail = c.finish().expect("tail");
        assert_eq!(tail.len(), 500);
        assert_eq!(tail[0], 2_000);
        assert!(c.finish().is_none());
    }

    #[test]
    fn odd_byte_reads_carry_across_push_boundaries() {
        // Split one i16 across two reads: no sample lost, none invented.
        let mut c = PcmStreamChunker::new(8_000, 8_000);
        let samples: Vec<i16> = (0..2_001).map(|i| i as i16).collect();
        let bytes = le_bytes(&samples);
        let (a, b) = bytes.split_at(3); // 1 whole sample + a dangling low byte
        assert!(c.push(a).is_empty());
        let blocks = c.push(b);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].len(), 2_000);
        assert_eq!(blocks[0][..3], [0, 1, 2]); // the split sample survived intact
        let tail = c.finish().expect("tail");
        assert_eq!(tail, vec![2_000]);
    }

    #[test]
    fn rate_mismatch_resamples_each_block_to_the_sco_rate() {
        // 24 kHz source → 8 kHz SCO: a 6000-sample source block (~250 ms)
        // lands as ~2000 target samples — the resample decision is per block.
        let mut c = PcmStreamChunker::new(24_000, 8_000);
        let samples: Vec<i16> = vec![100; 6_000];
        let blocks = c.push(&le_bytes(&samples));
        assert_eq!(blocks.len(), 1);
        let n = blocks[0].len();
        assert!((1_900..=2_100).contains(&n), "got {n} samples");
        assert!(blocks[0].iter().all(|&s| (s - 100).abs() <= 1));
    }

    #[test]
    fn trailing_odd_byte_is_dropped_not_fabricated() {
        let mut c = PcmStreamChunker::new(8_000, 8_000);
        assert!(c.push(&[0x01]).is_empty()); // half a sample only
        assert!(c.finish().is_none());
    }

    #[test]
    fn stt_multipart_form_builds_with_wav_part() {
        let wav = crate::speech_wire::encode_wav_pcm16_mono(&[0i16; 160], 16_000);
        assert!(super::stt_multipart_form(wav).is_ok());
    }
}
