//! Call-scoped PCM bridge to FormLogic Desktop's OpenAI Realtime provider.
//!
//! This module deliberately knows nothing about an OpenAI API key. Aokie may
//! connect only to the exact Desktop loopback route and authenticates that
//! hop with `FORMLOGIC_AI_GATEWAY_TOKEN`. Desktop owns the upstream provider
//! credential and the OpenAI Realtime session.

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

pub const WIRE_SAMPLE_RATE: u32 = 24_000;
pub const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;
const COMMAND_DEPTH: usize = 32;
const EVENT_DEPTH: usize = 64;
const MAX_BINARY_BYTES: usize = 96_000; // two seconds of PCM16 at 24 kHz
const MAX_TEXT_BYTES: usize = 64 * 1024;
const INPUT_BATCH_SAMPLES: usize = 960; // 40 ms at the 24 kHz wire rate
                                        // Realtime providers commonly generate a complete spoken response much faster
                                        // than it can be played over SCO. Four seconds was too small even for a normal
                                        // greeting and turned a healthy provider burst into a terminal call failure.
                                        // Thirty seconds still bounds the reservoir to about 0.96 MiB at 16 kHz while
                                        // comfortably covering the configured short-response default. The physical
                                        // SCO queue remains independently paced to OUTPUT_LEAD_MS.
const MAX_OUTPUT_BUFFER_MS: u64 = 30_000;
pub const OUTPUT_LEAD_MS: u64 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDetection {
    ServerVad,
    SemanticVad,
}

impl TurnDetection {
    fn as_str(self) -> &'static str {
        match self {
            Self::ServerVad => "server_vad",
            Self::SemanticVad => "semantic_vad",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub endpoint: String,
    pub call_id: String,
    pub generation: u64,
    pub expected_destination: String,
    pub instructions: String,
    pub greeting: String,
    pub voice: Option<String>,
    pub model: Option<String>,
    pub turn_detection: TurnDetection,
    pub max_output_tokens: u32,
    /// Desktop may expose only its fixed, read-only business lookup tool when
    /// this trusted plugin capability bit is present.
    pub allow_business_lookup: bool,
    /// Desktop may expose its fixed appointment-request capture tool. The
    /// plugin validates the current call/transcript and emits only the
    /// dedicated durable event; this never grants generic record writes.
    pub allow_request_appointment: bool,
    /// The physical call-ending tool is advertised only when the operator has
    /// enabled agentHangup for this receptionist.
    pub allow_finish_call: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeEventKind {
    Ready {
        destination_origin: String,
    },
    SpeechStarted,
    InputTranscript {
        item_id: Option<String>,
        text: String,
    },
    OutputItemStarted {
        item_id: String,
    },
    OutputPcm {
        item_id: String,
        samples: Vec<i16>,
    },
    OutputTranscript {
        item_id: String,
        text: String,
    },
    OutputItemDone {
        item_id: String,
    },
    ToolCall {
        tool_call_id: String,
        name: String,
        arguments: Value,
    },
    HangupRequested {
        tool_call_id: String,
        response_id: String,
        item_id: String,
    },
    Error {
        code: Option<String>,
        message: String,
        fatal: bool,
        /// Exact output item abandoned by a recoverable error. The radio
        /// keeps the same tombstone until the matching item_done arrives.
        abandoned_item_id: Option<String>,
    },
    Closed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeEvent {
    pub call_id: String,
    pub generation: u64,
    pub kind: RealtimeEventKind,
}

#[derive(Debug)]
enum Command {
    Audio(Vec<u8>),
}

#[derive(Debug)]
enum ControlCommand {
    Begin,
    CancelOutput {
        item_id: String,
        played_ms: u64,
    },
    ToolResult {
        tool_call_id: String,
        name: String,
        ok: bool,
        output: Value,
        continue_response: bool,
    },
    Stop {
        reason: String,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: &'a str,
    generation: u64,
    destination_origin: &'a str,
    instructions: &'a str,
    greeting: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    turn_detection: &'static str,
    max_output_tokens: u32,
    input_format: &'static str,
    output_format: &'static str,
    sample_rate: u32,
    allow_business_lookup: bool,
    allow_request_appointment: bool,
    allow_finish_call: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolResultEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: &'a str,
    generation: u64,
    tool_call_id: &'a str,
    name: &'a str,
    ok: bool,
    output: &'a Value,
    continue_response: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: &'a str,
    generation: u64,
    item_id: &'a str,
    played_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BeginEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: &'a str,
    generation: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StopEvent<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: &'a str,
    generation: u64,
    reason: &'a str,
}

/// A validated Desktop provider URL. The provider id is returned for health
/// diagnostics without preserving or logging credentials (the URL cannot
/// contain any).
pub fn validate_endpoint(endpoint: &str) -> Result<String, String> {
    let endpoint = endpoint.trim();
    let parsed = Url::parse(endpoint).map_err(|e| format!("invalid realtime endpoint: {e}"))?;
    if parsed.scheme() != "ws"
        || parsed.host_str() != Some("127.0.0.1")
        || parsed.port() != Some(17_872)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(
            "realtime endpoint must be the exact ws://127.0.0.1:17872 Desktop provider route"
                .to_string(),
        );
    }
    let path = parsed.path();
    let prefix = "/api/ai/providers/";
    let suffix = "/v1/realtime/stream";
    let Some(provider) = path
        .strip_prefix(prefix)
        .and_then(|path| path.strip_suffix(suffix))
    else {
        return Err(
            "realtime endpoint path must be /api/ai/providers/{id}/v1/realtime/stream".to_string(),
        );
    };
    if provider.is_empty()
        || provider.len() > 128
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("realtime provider id contains unsupported characters".to_string());
    }
    Ok(provider.to_string())
}

/// Normalize the actual upstream processor disclosed by Desktop source
/// discovery. Caller PCM may leave this machine only for this persisted,
/// consented HTTPS origin, and Desktop must echo it when the stream is ready.
pub fn validate_destination_origin(destination: &str) -> Result<String, String> {
    let parsed = Url::parse(destination.trim())
        .map_err(|e| format!("invalid realtime destination origin: {e}"))?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(
            "realtime destination must be an HTTPS origin without credentials, path, query, or fragment"
                .to_string(),
        );
    }
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" {
        return Err("realtime destination has no canonical origin".to_string());
    }
    Ok(origin)
}

fn gateway_token_after_validation(endpoint: &str) -> Result<String, String> {
    validate_endpoint(endpoint)?;
    std::env::var("FORMLOGIC_AI_GATEWAY_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "FORMLOGIC_AI_GATEWAY_TOKEN is unavailable".to_string())
}

pub struct RealtimeVoiceSession {
    call_id: String,
    generation: u64,
    audio_tx: SyncSender<Command>,
    control_tx: Sender<ControlCommand>,
    event_rx: Receiver<RealtimeEvent>,
    input_resampler: StreamingResampler,
    input_batch: VecDeque<i16>,
}

impl RealtimeVoiceSession {
    pub fn spawn(config: SessionConfig) -> Result<Self, String> {
        validate_endpoint(&config.endpoint)?;
        if config.call_id.trim().is_empty() {
            return Err("realtime call id is empty".to_string());
        }
        let expected_destination = validate_destination_origin(&config.expected_destination)?;
        if expected_destination != config.expected_destination {
            return Err(
                "realtime destination must be stored as a canonical HTTPS origin".to_string(),
            );
        }
        let token = gateway_token_after_validation(&config.endpoint)?;
        let (audio_tx, audio_rx) = mpsc::sync_channel(COMMAND_DEPTH);
        // Safety controls never share capacity with PCM. A full audio queue
        // must not delay barge-in cancellation or call teardown.
        let (control_tx, control_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::sync_channel(EVENT_DEPTH);
        let call_id = config.call_id.clone();
        let generation = config.generation;
        std::thread::Builder::new()
            .name("aokie-realtime-voice".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| format!("realtime runtime failed: {e}"))?;
                    runtime.block_on(run_socket(
                        config.clone(),
                        token,
                        audio_rx,
                        control_rx,
                        event_tx.clone(),
                    ))
                });
                let reason = match result {
                    Ok(Ok(())) => "Desktop realtime stream closed".to_string(),
                    Ok(Err(error)) => error,
                    Err(_) => "Desktop realtime stream panicked".to_string(),
                };
                let _ = event_tx.try_send(RealtimeEvent {
                    call_id: config.call_id,
                    generation: config.generation,
                    kind: RealtimeEventKind::Closed { reason },
                });
            })
            .map_err(|e| format!("cannot start realtime voice worker: {e}"))?;
        Ok(Self {
            call_id,
            generation,
            audio_tx,
            control_tx,
            event_rx,
            input_resampler: StreamingResampler::new(WIRE_SAMPLE_RATE, WIRE_SAMPLE_RATE),
            input_batch: VecDeque::new(),
        })
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Resample SCO to 24 kHz and coalesce tiny hardware packets into ordered
    /// 40 ms PCM16LE commands. Queue pressure remains a hard failure: silently
    /// dropping caller audio would let the model answer a different utterance.
    pub fn send_input(&mut self, samples: &[i16], sample_rate: u32) -> Result<(), String> {
        if sample_rate == 0 || samples.is_empty() {
            return Ok(());
        }
        if self.input_resampler.from_rate() != sample_rate {
            self.input_resampler = StreamingResampler::new(sample_rate, WIRE_SAMPLE_RATE);
        }
        self.input_batch
            .extend(self.input_resampler.process(samples));
        while self.input_batch.len() >= INPUT_BATCH_SAMPLES {
            let mut bytes = Vec::with_capacity(INPUT_BATCH_SAMPLES * 2);
            for sample in self.input_batch.iter().take(INPUT_BATCH_SAMPLES) {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            self.audio_tx
                .try_send(Command::Audio(bytes))
                .map_err(|error| match error {
                    TrySendError::Full(_) => {
                        "Desktop realtime input queue is full; caller audio was not sent"
                            .to_string()
                    }
                    TrySendError::Disconnected(_) => {
                        "Desktop realtime input stream is closed".to_string()
                    }
                })?;
            self.input_batch.drain(..INPUT_BATCH_SAMPLES);
        }
        Ok(())
    }

    pub fn cancel_output(&self, item_id: &str, played_ms: u64) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::CancelOutput {
                item_id: item_id.to_string(),
                played_ms,
            })
            .map_err(|_| "Desktop realtime control queue is unavailable".to_string())
    }

    /// Complete exactly one Desktop-relayed, allow-listed provider tool call.
    /// The socket worker retains the provider call id and Desktop rejects a
    /// duplicate, stale, renamed, or cross-call result.
    pub fn complete_tool(
        &self,
        tool_call_id: &str,
        name: &str,
        ok: bool,
        output: Value,
        continue_response: bool,
    ) -> Result<(), String> {
        if tool_call_id.is_empty()
            || tool_call_id.len() > 256
            || name.is_empty()
            || name.len() > 64
            || tool_call_id.chars().any(char::is_control)
            || name.chars().any(char::is_control)
        {
            return Err("Desktop realtime tool result identity is invalid".to_string());
        }
        let encoded = serde_json::to_vec(&output)
            .map_err(|_| "Desktop realtime tool result is not JSON".to_string())?;
        if encoded.len() > MAX_TOOL_OUTPUT_BYTES {
            return Err("Desktop realtime tool result exceeded 8 KiB".to_string());
        }
        self.control_tx
            .send(ControlCommand::ToolResult {
                tool_call_id: tool_call_id.to_string(),
                name: name.to_string(),
                ok,
                output,
                continue_response,
            })
            .map_err(|_| "Desktop realtime control queue is unavailable".to_string())
    }

    /// Arm the already-ready upstream session. The caller must send this only
    /// after the exact call is active, SCO is up, screening has completed,
    /// and Aokie still owns the media fence. Merely opening the WebSocket must
    /// never generate a greeting while the phone is still ringing.
    pub fn begin(&self) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::Begin)
            .map_err(|_| "Desktop realtime control queue is unavailable".to_string())
    }

    pub fn stop(&self, reason: &str) {
        let _ = self.control_tx.send(ControlCommand::Stop {
            reason: reason.chars().take(200).collect(),
        });
    }

    pub fn try_recv(&self) -> Result<Option<RealtimeEvent>, String> {
        match self.event_rx.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err("Desktop realtime event stream is closed".to_string())
            }
        }
    }
}

async fn run_socket(
    config: SessionConfig,
    token: String,
    audio_rx: Receiver<Command>,
    control_rx: Receiver<ControlCommand>,
    event_tx: SyncSender<RealtimeEvent>,
) -> Result<(), String> {
    // Endpoint validation MUST precede token materialisation/attachment.
    validate_endpoint(&config.endpoint)?;
    let mut request = config
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|e| format!("cannot build realtime WebSocket request: {e}"))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "Desktop gateway token is not a valid HTTP header value".to_string())?;
    request.headers_mut().insert(AUTHORIZATION, bearer);
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("Desktop realtime connection failed: {e}"))?;
    let (mut sink, mut stream) = socket.split();
    let start = StartEvent {
        kind: "formlogic.realtime.start",
        call_id: &config.call_id,
        generation: config.generation,
        destination_origin: &config.expected_destination,
        instructions: &config.instructions,
        greeting: &config.greeting,
        voice: config.voice.as_deref(),
        model: config.model.as_deref(),
        turn_detection: config.turn_detection.as_str(),
        max_output_tokens: config.max_output_tokens.clamp(32, 4_096),
        input_format: "pcm16",
        output_format: "pcm16",
        sample_rate: WIRE_SAMPLE_RATE,
        allow_business_lookup: config.allow_business_lookup,
        allow_request_appointment: config.allow_request_appointment,
        allow_finish_call: config.allow_finish_call,
    };
    sink.send(Message::Text(
        serde_json::to_string(&start)
            .map_err(|e| format!("cannot encode realtime start event: {e}"))?
            .into(),
    ))
    .await
    .map_err(|e| format!("cannot start Desktop realtime stream: {e}"))?;

    let mut output_state = OutputParseState::default();
    let mut begun = false;
    let mut command_poll = tokio::time::interval(Duration::from_millis(5));
    command_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = command_poll.tick() => {
                // Stop/cancel always preempt queued audio.
                loop {
                    match control_rx.try_recv() {
                        Ok(ControlCommand::Begin) => {
                            if begun {
                                return Err("Desktop realtime session was begun more than once".to_string());
                            }
                            begun = true;
                            let event = BeginEvent {
                                kind: "formlogic.realtime.begin",
                                call_id: &config.call_id,
                                generation: config.generation,
                            };
                            sink.send(Message::Text(serde_json::to_string(&event).unwrap().into())).await
                                .map_err(|e| format!("Desktop realtime begin failed: {e}"))?;
                        }
                        Ok(ControlCommand::CancelOutput { item_id, played_ms }) => {
                            if !begun {
                                return Err("Desktop realtime cancel arrived before begin".to_string());
                            }
                            let event = CancelEvent {
                                kind: "formlogic.realtime.cancel_output",
                                call_id: &config.call_id,
                                generation: config.generation,
                                item_id: &item_id,
                                played_ms,
                            };
                            sink.send(Message::Text(serde_json::to_string(&event).unwrap().into())).await
                                .map_err(|e| format!("Desktop realtime cancel failed: {e}"))?;
                            // Raw binary frames carry no item id. Once an
                            // exact item is cancelled, retain its identity in
                            // the parser until Desktop sends the matching
                            // item_done so late PCM can never be mistaken for
                            // a future response.
                            output_state.abandon_exact(&item_id)?;
                        }
                        Ok(ControlCommand::ToolResult {
                            tool_call_id,
                            name,
                            ok,
                            output,
                            continue_response,
                        }) => {
                            if !begun {
                                return Err("Desktop realtime tool result arrived before begin".to_string());
                            }
                            let event = ToolResultEvent {
                                kind: "formlogic.realtime.tool_result",
                                call_id: &config.call_id,
                                generation: config.generation,
                                tool_call_id: &tool_call_id,
                                name: &name,
                                ok,
                                output: &output,
                                continue_response,
                            };
                            sink.send(Message::Text(serde_json::to_string(&event).unwrap().into())).await
                                .map_err(|e| format!("Desktop realtime tool result failed: {e}"))?;
                        }
                        Ok(ControlCommand::Stop { reason }) => {
                            let event = StopEvent {
                                kind: "formlogic.realtime.stop",
                                call_id: &config.call_id,
                                generation: config.generation,
                                reason: &reason,
                            };
                            let _ = sink.send(Message::Text(serde_json::to_string(&event).unwrap().into())).await;
                            let _ = sink.close().await;
                            return Ok(());
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return Ok(()),
                    }
                }
                for _ in 0..8 {
                    match audio_rx.try_recv() {
                        Ok(Command::Audio(bytes)) => {
                            if !begun {
                                return Err("caller audio was queued before Desktop realtime begin".to_string());
                            }
                            sink.send(Message::Binary(bytes.into())).await
                                .map_err(|e| format!("Desktop realtime audio send failed: {e}"))?
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return Ok(()),
                    }
                }
            }
            message = stream.next() => {
                let Some(message) = message else { return Ok(()); };
                match message.map_err(|e| format!("Desktop realtime receive failed: {e}"))? {
                    Message::Text(text) => {
                        if text.len() > MAX_TEXT_BYTES {
                            return Err("Desktop realtime text event exceeded 64 KiB".to_string());
                        }
                        let kind = parse_server_text(
                            &text,
                            &config.call_id,
                            config.generation,
                            &config.expected_destination,
                            &mut output_state,
                        )?;
                        if let Some(kind) = kind {
                            send_event(&event_tx, &config, kind)?;
                        }
                    }
                    Message::Binary(bytes) => {
                        if bytes.is_empty() { continue; }
                        if bytes.len() > MAX_BINARY_BYTES || bytes.len() % 2 != 0 {
                            return Err("Desktop realtime PCM frame is malformed or too large".to_string());
                        }
                        let Some(item_id) = output_state.pcm_item()? else {
                            // A cancelled or recoverably-failed item remains
                            // fenced until its exact item_done. Its late PCM
                            // is expected and must not close the call.
                            continue;
                        };
                        let samples = bytes
                            .chunks_exact(2)
                            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                            .collect();
                        send_event(&event_tx, &config, RealtimeEventKind::OutputPcm { item_id, samples })?;
                    }
                    Message::Ping(payload) => sink.send(Message::Pong(payload)).await
                        .map_err(|e| format!("Desktop realtime pong failed: {e}"))?,
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(frame) => {
                        return Err(frame
                            .map(|frame| format!("Desktop realtime closed: {}", frame.reason))
                            .unwrap_or_else(|| "Desktop realtime closed".to_string()));
                    }
                }
            }
        }
    }
}

/// Parser-side ownership for raw provider audio.
///
/// Desktop deliberately sends PCM as compact binary frames, so those frames
/// do not carry an item id of their own. An abandoned item therefore cannot
/// simply be cleared on cancellation or a recoverable response error: doing
/// so would turn its legal late PCM/item_done tail into a fatal protocol
/// error, or worse, attribute it to a newer response. Keep one exact
/// tombstone until the ordered matching item_done arrives.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct OutputParseState {
    active: Option<String>,
    abandoned: Option<String>,
}

impl OutputParseState {
    fn start(&mut self, item_id: &str) -> Result<(), String> {
        if self.abandoned.is_some() {
            return Err(
                "Desktop realtime started output before the abandoned item completed".to_string(),
            );
        }
        if self
            .active
            .as_deref()
            .is_some_and(|active| active != item_id)
        {
            return Err("Desktop realtime overlapped output items".to_string());
        }
        self.active = Some(item_id.to_string());
        Ok(())
    }

    fn abandon_current(&mut self) -> Option<String> {
        if let Some(item_id) = self.active.take() {
            self.abandoned = Some(item_id);
        }
        self.abandoned.clone()
    }

    fn abandon_exact(&mut self, item_id: &str) -> Result<(), String> {
        match (self.active.as_deref(), self.abandoned.as_deref()) {
            (Some(active), None) if active == item_id => {
                self.abandoned = self.active.take();
                Ok(())
            }
            (Some(_), None) => Err(
                "Desktop realtime cancellation did not match the active output item".to_string(),
            ),
            (None, Some(abandoned)) if abandoned == item_id => Ok(()),
            (None, Some(_)) => {
                Err("Desktop realtime cancellation crossed an abandoned output item".to_string())
            }
            // The provider may already have completed the item while its SCO
            // playout tail is still active. Desktop retains that completed
            // identity for truncation, so an idle parser is valid here.
            (None, None) => Ok(()),
            (Some(_), Some(_)) => {
                Err("Desktop realtime output parser entered an invalid state".to_string())
            }
        }
    }

    /// `Ok(None)` means the binary frame belongs to the exact abandoned item
    /// and is intentionally dropped.
    fn pcm_item(&self) -> Result<Option<String>, String> {
        match (self.active.as_ref(), self.abandoned.as_ref()) {
            (Some(item_id), None) => Ok(Some(item_id.clone())),
            (None, Some(_)) => Ok(None),
            (None, None) => {
                Err("Desktop realtime sent PCM without an active output item".to_string())
            }
            (Some(_), Some(_)) => {
                Err("Desktop realtime output parser entered an invalid state".to_string())
            }
        }
    }

    /// Returns true when the transcript belongs to the active item, or false
    /// when it is the legal late tail of the exact abandoned item.
    fn accepts_transcript(&self, item_id: &str) -> Result<bool, String> {
        if self.active.as_deref() == Some(item_id) {
            return Ok(true);
        }
        if self.abandoned.as_deref() == Some(item_id) {
            return Ok(false);
        }
        Err("Desktop realtime transcript did not match the current output item".to_string())
    }

    fn finish(&mut self, item_id: &str) -> Result<(), String> {
        if self.active.as_deref() == Some(item_id) {
            self.active = None;
            return Ok(());
        }
        if self.abandoned.as_deref() == Some(item_id) {
            self.abandoned = None;
            return Ok(());
        }
        Err("Desktop realtime completed a non-active output item".to_string())
    }
}

fn send_event(
    tx: &SyncSender<RealtimeEvent>,
    config: &SessionConfig,
    kind: RealtimeEventKind,
) -> Result<(), String> {
    tx.try_send(RealtimeEvent {
        call_id: config.call_id.clone(),
        generation: config.generation,
        kind,
    })
    .map_err(|error| match error {
        TrySendError::Full(_) => "Desktop realtime output queue overflowed".to_string(),
        TrySendError::Disconnected(_) => "Desktop realtime consumer stopped".to_string(),
    })
}

fn parse_server_text(
    text: &str,
    call_id: &str,
    generation: u64,
    expected_destination: &str,
    output_state: &mut OutputParseState,
) -> Result<Option<RealtimeEventKind>, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("Desktop realtime sent invalid JSON: {e}"))?;
    let server_call_id = value
        .get("callId")
        .and_then(Value::as_str)
        .ok_or_else(|| "Desktop realtime event omitted callId".to_string())?;
    if server_call_id != call_id {
        return Err("Desktop realtime event carried a stale call id".to_string());
    }
    let server_generation = value
        .get("generation")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Desktop realtime event omitted generation".to_string())?;
    if server_generation != generation {
        return Err("Desktop realtime event carried a stale generation".to_string());
    }
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "Desktop realtime event has no type".to_string())?;
    let item_id = || {
        value
            .get("itemId")
            .or_else(|| value.get("item_id"))
            .and_then(Value::as_str)
            .filter(|item| !item.is_empty() && item.len() <= 256)
            .map(str::to_string)
            .ok_or_else(|| "Desktop realtime output event has no valid itemId".to_string())
    };
    let transcript = || {
        value
            .get("transcript")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .map(|text| text.chars().take(16_000).collect::<String>())
            .ok_or_else(|| "Desktop realtime transcript event has no text".to_string())
    };
    match kind {
        "formlogic.realtime.ready" | "ready" => {
            let destination = value
                .get("destinationOrigin")
                .or_else(|| value.get("destination_origin"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    "Desktop realtime ready event omitted destinationOrigin".to_string()
                })?;
            let destination = validate_destination_origin(destination)?;
            if destination != expected_destination {
                return Err(format!(
                    "Desktop realtime destination changed (expected {expected_destination}, got {destination})"
                ));
            }
            Ok(Some(RealtimeEventKind::Ready {
                destination_origin: destination,
            }))
        }
        "formlogic.realtime.speech_started" | "speech_started" => {
            Ok(Some(RealtimeEventKind::SpeechStarted))
        }
        "formlogic.realtime.input_transcript" | "input_transcript" => {
            let input_item = value
                .get("itemId")
                .or_else(|| value.get("item_id"))
                .and_then(Value::as_str)
                .map(|item| item.chars().take(256).collect());
            if value.get("final").and_then(Value::as_bool) == Some(false) {
                return Ok(None);
            }
            Ok(Some(RealtimeEventKind::InputTranscript {
                item_id: input_item,
                text: transcript()?,
            }))
        }
        "formlogic.realtime.output_item_started" | "output_item_started" => {
            let item_id = item_id()?;
            output_state.start(&item_id)?;
            Ok(Some(RealtimeEventKind::OutputItemStarted { item_id }))
        }
        "formlogic.realtime.output_transcript" | "output_transcript" => {
            if value.get("final").and_then(Value::as_bool) == Some(false) {
                return Ok(None);
            }
            let item_id = item_id()?;
            if output_state.accepts_transcript(&item_id)? {
                Ok(Some(RealtimeEventKind::OutputTranscript {
                    item_id,
                    text: transcript()?,
                }))
            } else {
                Ok(None)
            }
        }
        "formlogic.realtime.output_item_done" | "output_item_done" => {
            let item_id = item_id()?;
            output_state.finish(&item_id)?;
            Ok(Some(RealtimeEventKind::OutputItemDone { item_id }))
        }
        "formlogic.realtime.tool_call" => {
            let tool_call_id = value
                .get("toolCallId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
                .ok_or_else(|| "Desktop realtime tool call has no valid toolCallId".to_string())?
                .to_string();
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| {
                    matches!(
                        *name,
                        "lookup_business_data" | "request_appointment" | "finish_call"
                    )
                })
                .ok_or_else(|| "Desktop realtime requested an unsupported tool".to_string())?
                .to_string();
            let arguments = value
                .get("arguments")
                .filter(|arguments| arguments.is_object())
                .cloned()
                .ok_or_else(|| "Desktop realtime tool arguments are invalid".to_string())?;
            if serde_json::to_vec(&arguments)
                .map_err(|_| "Desktop realtime tool arguments are not JSON".to_string())?
                .len()
                > 8 * 1024
            {
                return Err("Desktop realtime tool arguments exceeded 8 KiB".to_string());
            }
            Ok(Some(RealtimeEventKind::ToolCall {
                tool_call_id,
                name,
                arguments,
            }))
        }
        "formlogic.realtime.hangup_requested" => {
            let bounded_id = |field: &str| {
                value
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|id| {
                        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
                    })
                    .map(str::to_string)
                    .ok_or_else(|| format!("Desktop realtime hangup omitted valid {field}"))
            };
            Ok(Some(RealtimeEventKind::HangupRequested {
                tool_call_id: bounded_id("toolCallId")?,
                response_id: bounded_id("responseId")?,
                item_id: bounded_id("itemId")?,
            }))
        }
        "formlogic.realtime.error" | "error" => {
            let fatal = value.get("fatal").and_then(Value::as_bool).unwrap_or(true);
            let abandoned_item_id = if !fatal {
                // Preserve the affected item as a parser tombstone. Desktop
                // retains it until the provider's exact item_done, and raw
                // late PCM has no item id with which to fence itself.
                output_state.abandon_current()
            } else {
                None
            };
            Ok(Some(RealtimeEventKind::Error {
                code: value
                    .get("code")
                    .and_then(Value::as_str)
                    .map(|code| code.chars().take(100).collect()),
                message: value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Desktop realtime provider error")
                    .chars()
                    .take(1_000)
                    .collect(),
                fatal,
                abandoned_item_id,
            }))
        }
        other => Err(format!("unsupported Desktop realtime event: {other}")),
    }
}

/// Stateful linear PCM resampler. Keeping phase and the previous sample
/// across chunks prevents the boundary clicks and cumulative drift caused by
/// independently resampling every SCO packet.
#[derive(Debug, Clone)]
pub struct StreamingResampler {
    from: u32,
    to: u32,
    previous: Option<i16>,
    input_index: u64,
    next_output_position: u64,
}

impl StreamingResampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from: from.max(1),
            to: to.max(1),
            previous: None,
            input_index: 0,
            next_output_position: 0,
        }
    }

    pub fn from_rate(&self) -> u32 {
        self.from
    }

    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let mut output = Vec::with_capacity(
            input.len().saturating_mul(self.to as usize) / self.from as usize + 2,
        );
        for &current in input {
            if let Some(previous) = self.previous {
                let right = self.input_index.saturating_mul(self.to as u64);
                let left = right.saturating_sub(self.to as u64);
                while self.next_output_position <= right {
                    let alpha = self
                        .next_output_position
                        .saturating_sub(left)
                        .min(self.to as u64);
                    let delta = current as i64 - previous as i64;
                    let interpolated = previous as i64
                        + (delta * alpha as i64 + (self.to as i64 / 2)) / self.to as i64;
                    output.push(interpolated.clamp(i16::MIN as i64, i16::MAX as i64) as i16);
                    self.next_output_position =
                        self.next_output_position.saturating_add(self.from as u64);
                }
            } else {
                output.push(current);
                self.next_output_position = self.from as u64;
            }
            self.previous = Some(current);
            self.input_index = self.input_index.saturating_add(1);
        }
        output
    }
}

/// Bounds how far Aokie may queue provider audio into the SCO backend. The
/// backend itself has a large queue, so pacing must happen before `send_audio`.
#[derive(Debug)]
pub struct OutputPacer {
    sample_rate: u32,
    queued: VecDeque<i16>,
    started_at: Option<Instant>,
    sent_samples: u64,
    active_item: Option<String>,
    item_done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputPacerPushError {
    StaleItem,
    CapacityExceeded,
}

impl std::fmt::Display for OutputPacerPushError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleItem => {
                formatter.write_str("Desktop realtime PCM belongs to a stale output item")
            }
            Self::CapacityExceeded => formatter
                .write_str("Desktop realtime output exceeded the 30 second playout reservoir"),
        }
    }
}

impl OutputPacer {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            queued: VecDeque::new(),
            started_at: None,
            sent_samples: 0,
            active_item: None,
            item_done: false,
        }
    }

    pub fn reset_rate(&mut self, sample_rate: u32) {
        if self.sample_rate != sample_rate.max(1) {
            self.clear();
            self.sample_rate = sample_rate.max(1);
        }
    }

    pub fn start_item(&mut self, item_id: &str, _now: Instant) -> Result<(), String> {
        if self
            .active_item
            .as_deref()
            .is_some_and(|active| active != item_id)
            || !self.queued.is_empty()
        {
            return Err("Desktop realtime attempted overlapping output playout".to_string());
        }
        self.active_item = Some(item_id.to_string());
        self.item_done = false;
        // The server may announce an item well before its first audio delta.
        // Starting the clock here would turn that network/generation delay
        // into false playout allowance and could dump hundreds of ms into SCO.
        self.started_at = None;
        self.sent_samples = 0;
        Ok(())
    }

    pub fn push(&mut self, item_id: &str, samples: &[i16]) -> Result<(), OutputPacerPushError> {
        if self.active_item.as_deref() != Some(item_id) {
            return Err(OutputPacerPushError::StaleItem);
        }
        let max_samples = self.sample_rate as usize * MAX_OUTPUT_BUFFER_MS as usize / 1_000;
        if self.queued.len().saturating_add(samples.len()) > max_samples {
            return Err(OutputPacerPushError::CapacityExceeded);
        }
        self.queued.extend(samples.iter().copied());
        Ok(())
    }

    pub fn finish_item(&mut self, item_id: &str) -> Result<(), String> {
        if self.active_item.as_deref() != Some(item_id) {
            return Err("Desktop realtime finished a stale output item".to_string());
        }
        self.item_done = true;
        if self.queued.is_empty() {
            self.active_item = None;
            self.item_done = false;
            self.started_at = None;
        }
        Ok(())
    }

    pub fn take_ready(&mut self, now: Instant, max_samples: usize) -> Vec<i16> {
        if self.queued.is_empty() {
            return Vec::new();
        }
        let started_at = *self.started_at.get_or_insert(now);
        let elapsed_samples =
            now.saturating_duration_since(started_at).as_secs_f64() * self.sample_rate as f64;
        let lead_samples = self.sample_rate as u64 * OUTPUT_LEAD_MS / 1_000;
        let allowed_total = elapsed_samples as u64 + lead_samples;
        let allowed_now = allowed_total.saturating_sub(self.sent_samples) as usize;
        let take = allowed_now.min(max_samples).min(self.queued.len());
        let mut output = Vec::with_capacity(take);
        output.extend(self.queued.drain(..take));
        self.sent_samples = self.sent_samples.saturating_add(output.len() as u64);
        if self.queued.is_empty() && self.item_done {
            self.active_item = None;
            self.item_done = false;
            self.started_at = None;
        }
        output
    }

    /// Audio provably audible to the caller. Samples inside the allowed SCO
    /// lead are excluded because `flush_tx_audio` will discard them during an
    /// interruption; reporting them as heard would over-truncate model state.
    pub fn audible_played_ms(&self, now: Instant) -> u64 {
        let Some(started_at) = self.started_at else {
            return 0;
        };
        let elapsed_samples =
            now.saturating_duration_since(started_at).as_secs_f64() * self.sample_rate as f64;
        self.sent_samples
            .min(elapsed_samples as u64)
            .saturating_mul(1_000)
            / self.sample_rate as u64
    }

    pub fn active_item(&self) -> Option<&str> {
        self.active_item.as_deref()
    }

    pub fn mark_drained_if_done(&mut self, item_id: &str) {
        if self.active_item.as_deref() == Some(item_id) && self.queued.is_empty() && self.item_done
        {
            self.active_item = None;
            self.item_done = false;
            self.started_at = None;
        }
    }

    pub fn clear(&mut self) {
        self.queued.clear();
        self.started_at = None;
        self.sent_samples = 0;
        self.active_item = None;
        self.item_done = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_exact_and_never_accepts_lookalikes() {
        let good = "ws://127.0.0.1:17872/api/ai/providers/openai-realtime-mini/v1/realtime/stream";
        assert_eq!(validate_endpoint(good).unwrap(), "openai-realtime-mini");
        for bad in [
            "wss://127.0.0.1:17872/api/ai/providers/openai/v1/realtime/stream",
            "ws://localhost:17872/api/ai/providers/openai/v1/realtime/stream",
            "ws://127.0.0.1:17873/api/ai/providers/openai/v1/realtime/stream",
            "ws://user:secret@127.0.0.1:17872/api/ai/providers/openai/v1/realtime/stream",
            "ws://127.0.0.1:17872/api/ai/providers/openai/v1/realtime/stream?x=1",
            "ws://127.0.0.1:17872/api/ai/providers/openai%2Frealtime/v1/realtime/stream",
            "ws://127.0.0.1:17872/api/ai/providers/openai/v1/realtime/voice",
        ] {
            assert!(validate_endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resampler_keeps_phase_across_sco_chunks() {
        let input: Vec<i16> = (0..1_600).map(|n| ((n % 200) as i16) * 100).collect();
        let mut whole = StreamingResampler::new(16_000, 24_000);
        let expected = whole.process(&input);
        let mut chunked = StreamingResampler::new(16_000, 24_000);
        let mut actual = Vec::new();
        for chunk in input.chunks(37) {
            actual.extend(chunked.process(chunk));
        }
        assert_eq!(actual, expected);
        assert!((actual.len() as isize - 2_400).abs() <= 2);
    }

    #[test]
    fn resampler_supports_telephony_both_directions() {
        for rate in [8_000, 16_000] {
            let input = vec![1_000_i16; rate as usize / 10];
            let mut up = StreamingResampler::new(rate, WIRE_SAMPLE_RATE);
            let wire = up.process(&input);
            assert!((wire.len() as isize - 2_400).abs() <= 2);
            let mut down = StreamingResampler::new(WIRE_SAMPLE_RATE, rate);
            let output = down.process(&wire);
            assert!((output.len() as isize - input.len() as isize).abs() <= 2);
            assert!(output.iter().all(|sample| *sample == 1_000));
        }
    }

    #[test]
    fn input_batches_tiny_sco_frames_and_keeps_a_time_sized_queue() {
        let (audio_tx, audio_rx) = mpsc::sync_channel(COMMAND_DEPTH);
        let (control_tx, _control_rx) = mpsc::channel();
        let (_event_tx, event_rx) = mpsc::sync_channel(EVENT_DEPTH);
        let mut session = RealtimeVoiceSession {
            call_id: "call_1".into(),
            generation: 7,
            audio_tx,
            control_tx,
            event_rx,
            input_resampler: StreamingResampler::new(WIRE_SAMPLE_RATE, WIRE_SAMPLE_RATE),
            input_batch: VecDeque::new(),
        };

        // Five 8 ms frames become one ordered 40 ms wire command rather than
        // five queue entries. This turns depth 32 from ~240 ms on the live
        // 7.5 ms SCO cadence into a deterministic 1.28 second reservoir.
        let mut expected = Vec::new();
        for value in 0..5i16 {
            let frame = vec![value; 192];
            expected.extend_from_slice(&frame);
            session.send_input(&frame, WIRE_SAMPLE_RATE).unwrap();
            if value < 4 {
                assert!(matches!(audio_rx.try_recv(), Err(TryRecvError::Empty)));
            }
        }
        let Command::Audio(bytes) = audio_rx.try_recv().unwrap();
        let actual: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(actual, expected);
        assert!(session.input_batch.is_empty());
        assert_eq!(
            COMMAND_DEPTH * INPUT_BATCH_SAMPLES * 1_000 / WIRE_SAMPLE_RATE as usize,
            1_280
        );

        for _ in 0..COMMAND_DEPTH {
            session
                .send_input(&vec![0; INPUT_BATCH_SAMPLES], WIRE_SAMPLE_RATE)
                .unwrap();
        }
        assert!(session
            .send_input(&vec![0; INPUT_BATCH_SAMPLES], WIRE_SAMPLE_RATE)
            .unwrap_err()
            .contains("input queue is full"));
    }

    #[test]
    fn recoverable_error_tombstones_late_output_until_exact_done() {
        let mut state = OutputParseState::default();
        assert!(state.pcm_item().is_err());
        assert!(parse_server_text(
            r#"{"type":"formlogic.realtime.ready","callId":"other","generation":7}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .is_err());
        let started = parse_server_text(
            r#"{"type":"formlogic.realtime.output_item_started","callId":"call_1","generation":7,"itemId":"item_1"}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            started,
            RealtimeEventKind::OutputItemStarted {
                item_id: "item_1".into()
            }
        );
        assert_eq!(state.active.as_deref(), Some("item_1"));

        let recoverable = parse_server_text(
            r#"{"type":"formlogic.realtime.error","callId":"call_1","generation":7,"code":"response_failed","message":"try again","fatal":false}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            recoverable,
            RealtimeEventKind::Error {
                code: Some("response_failed".into()),
                message: "try again".into(),
                fatal: false,
                abandoned_item_id: Some("item_1".into()),
            }
        );
        assert!(state.active.is_none());
        assert_eq!(state.abandoned.as_deref(), Some("item_1"));
        assert_eq!(state.pcm_item().unwrap(), None);
        assert!(parse_server_text(
            r#"{"type":"formlogic.realtime.output_transcript","callId":"call_1","generation":7,"itemId":"item_1","transcript":"late","final":true}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .is_none());
        assert!(parse_server_text(
            r#"{"type":"formlogic.realtime.output_item_started","callId":"call_1","generation":7,"itemId":"item_2"}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .is_err());
        assert_eq!(
            parse_server_text(
                r#"{"type":"formlogic.realtime.output_item_done","callId":"call_1","generation":7,"itemId":"item_1"}"#,
                "call_1",
                7,
                "https://api.openai.com",
                &mut state,
            )
            .unwrap()
            .unwrap(),
            RealtimeEventKind::OutputItemDone {
                item_id: "item_1".into()
            }
        );
        assert_eq!(state, OutputParseState::default());
        assert!(parse_server_text(
            r#"{"type":"formlogic.realtime.output_item_started","callId":"call_1","generation":7,"itemId":"item_2"}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn cancelled_item_survives_recoverable_error_and_late_tail() {
        let mut state = OutputParseState::default();
        state.start("item_1").unwrap();
        state.abandon_exact("item_1").unwrap();
        let error = parse_server_text(
            r#"{"type":"formlogic.realtime.error","callId":"call_1","generation":7,"message":"cancel raced failure","fatal":false}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            error,
            RealtimeEventKind::Error {
                code: None,
                message: "cancel raced failure".into(),
                fatal: false,
                abandoned_item_id: Some("item_1".into()),
            }
        );
        assert_eq!(state.pcm_item().unwrap(), None);
        assert!(parse_server_text(
            r#"{"type":"formlogic.realtime.output_item_done","callId":"call_1","generation":7,"itemId":"item_1"}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .is_some());
        assert_eq!(state, OutputParseState::default());
    }

    #[test]
    fn incomplete_response_after_normal_item_done_keeps_session_available() {
        let mut state = OutputParseState::default();
        state.start("item_1").unwrap();
        state.finish("item_1").unwrap();
        let error = parse_server_text(
            r#"{"type":"formlogic.realtime.error","callId":"call_1","generation":7,"code":"response_failed","message":"incomplete","fatal":false}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            error,
            RealtimeEventKind::Error {
                fatal: false,
                abandoned_item_id: None,
                ..
            }
        ));
        state.start("item_2").unwrap();
        assert_eq!(state.pcm_item().unwrap().as_deref(), Some("item_2"));
    }

    #[test]
    fn ready_requires_the_consented_upstream_origin() {
        let mut state = OutputParseState::default();
        let ready = parse_server_text(
            r#"{"type":"formlogic.realtime.ready","callId":"call_1","generation":7,"destinationOrigin":"https://api.openai.com"}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            ready,
            RealtimeEventKind::Ready {
                destination_origin: "https://api.openai.com".into()
            }
        );
        assert!(parse_server_text(
            r#"{"type":"formlogic.realtime.ready","callId":"call_1","generation":7,"destinationOrigin":"https://other.example"}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .is_err());
        assert_eq!(
            validate_destination_origin("https://api.openai.com/").unwrap(),
            "https://api.openai.com"
        );
        assert!(validate_destination_origin("http://api.openai.com").is_err());
        assert!(validate_destination_origin("https://api.openai.com/v1").is_err());
    }

    #[test]
    fn tool_and_hangup_controls_are_exactly_fenced_and_allow_listed() {
        let start = serde_json::to_value(StartEvent {
            kind: "formlogic.realtime.start",
            call_id: "call_1",
            generation: 7,
            destination_origin: "https://api.openai.com",
            instructions: "Receptionist",
            greeting: "Hello",
            voice: None,
            model: None,
            turn_detection: "server_vad",
            max_output_tokens: 128,
            input_format: "pcm16",
            output_format: "pcm16",
            sample_rate: WIRE_SAMPLE_RATE,
            allow_business_lookup: true,
            allow_request_appointment: true,
            allow_finish_call: true,
        })
        .unwrap();
        assert_eq!(start["allowRequestAppointment"], true);

        let interrupted = ToolResultEvent {
            kind: "formlogic.realtime.tool_result",
            call_id: "call_1",
            generation: 7,
            tool_call_id: "tool_1",
            name: "lookup_business_data",
            ok: false,
            output: &serde_json::json!({ "error": "caller continued speaking" }),
            continue_response: false,
        };
        let interrupted = serde_json::to_value(interrupted).unwrap();
        assert_eq!(interrupted["continueResponse"], false);

        let mut state = OutputParseState::default();
        let tool = parse_server_text(
            r#"{"type":"formlogic.realtime.tool_call","callId":"call_1","generation":7,"toolCallId":"tool_1","name":"lookup_business_data","arguments":{"question":"Do I have anything on Tuesday?"}}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            tool,
            RealtimeEventKind::ToolCall {
                tool_call_id: "tool_1".into(),
                name: "lookup_business_data".into(),
                arguments: serde_json::json!({"question": "Do I have anything on Tuesday?"}),
            }
        );

        let appointment = parse_server_text(
            r#"{"type":"formlogic.realtime.tool_call","callId":"call_1","generation":7,"toolCallId":"tool_appointment","name":"request_appointment","arguments":{"callerName":"Lance","service":"Lawn mowing","date":"2026-07-22","time":"10:00","agreementPhrase":"Wednesday at 10."}}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            appointment,
            RealtimeEventKind::ToolCall {
                tool_call_id: "tool_appointment".into(),
                name: "request_appointment".into(),
                arguments: serde_json::json!({
                    "callerName": "Lance",
                    "service": "Lawn mowing",
                    "date": "2026-07-22",
                    "time": "10:00",
                    "agreementPhrase": "Wednesday at 10."
                }),
            }
        );

        assert!(parse_server_text(
            r#"{"type":"formlogic.realtime.tool_call","callId":"call_1","generation":8,"toolCallId":"tool_1","name":"lookup_business_data","arguments":{"question":"x"}}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .is_err());
        assert!(parse_server_text(
            r#"{"type":"formlogic.realtime.tool_call","callId":"call_1","generation":7,"toolCallId":"tool_2","name":"arbitrary_desktop_action","arguments":{}}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .is_err());

        let hangup = parse_server_text(
            r#"{"type":"formlogic.realtime.hangup_requested","callId":"call_1","generation":7,"toolCallId":"tool_3","responseId":"response_3","itemId":"item_3"}"#,
            "call_1",
            7,
            "https://api.openai.com",
            &mut state,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            hangup,
            RealtimeEventKind::HangupRequested {
                tool_call_id: "tool_3".into(),
                response_id: "response_3".into(),
                item_id: "item_3".into(),
            }
        );
    }

    #[test]
    fn pacer_accepts_fast_normal_output_caps_sco_lead_and_rejects_unbounded_output() {
        let now = Instant::now();
        let mut pacer = OutputPacer::new(16_000);
        pacer.start_item("item", now).unwrap();
        // A normal provider burst can contain substantially more than four
        // seconds of audio immediately; accepting it must not dump it into
        // SCO ahead of wall clock.
        pacer.push("item", &vec![0; 16_000 * 8]).unwrap();
        let first_pcm = now + Duration::from_millis(500);
        assert_eq!(pacer.take_ready(first_pcm, usize::MAX).len(), 1_280);
        assert_eq!(pacer.audible_played_ms(first_pcm), 0);
        let later = first_pcm + Duration::from_millis(200);
        assert_eq!(pacer.take_ready(later, usize::MAX).len(), 3_200);
        assert_eq!(pacer.audible_played_ms(later), 200);

        let capacity = 16_000 * MAX_OUTPUT_BUFFER_MS as usize / 1_000;
        let remaining = capacity - (16_000 * 8 - 1_280 - 3_200);
        pacer.push("item", &vec![0; remaining]).unwrap();
        assert_eq!(
            pacer.push("item", &[0]),
            Err(OutputPacerPushError::CapacityExceeded)
        );
        pacer.clear();
        assert!(pacer.active_item().is_none());
    }

    #[test]
    fn pacer_distinguishes_stale_pcm_from_a_recoverable_capacity_limit() {
        let mut pacer = OutputPacer::new(16_000);
        assert_eq!(
            pacer.push("stale", &[0]),
            Err(OutputPacerPushError::StaleItem)
        );
        pacer.start_item("current", Instant::now()).unwrap();
        pacer
            .push(
                "current",
                &vec![0; 16_000 * MAX_OUTPUT_BUFFER_MS as usize / 1_000],
            )
            .unwrap();
        assert_eq!(
            pacer.push("current", &[0]),
            Err(OutputPacerPushError::CapacityExceeded)
        );
    }
}
