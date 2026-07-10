//! Live Bluetooth radio integration.
//!
//! Runs the real `aokie_radio` `AokieRuntime` (via
//! [`aokie_dongle::bluetooth::BluetoothManager`]) on a dedicated background
//! thread, maps its `BluetoothEvent`s onto the `aokie.*` Desktop-event
//! contract (emitted straight to stdout **and** the durable outbox), and
//! accepts control requests â€” answer / reject / hangup / send-SMS / speak â€”
//! from the main RPC thread over an mpsc channel.
//!
//! ## Why a second `Outbox` + `StdoutSink` on this thread
//! The plugin's main loop blocks on stdin, so an asynchronous call event (a
//! call can ring at any instant) must be delivered without waiting for the
//! next RPC. `StdoutSink` writes one whole line under the stdout lock
//! (line-atomic), so this thread's sink and the main thread's sink never
//! interleave. The [`Outbox`] here is a *second* SQLite connection to the
//! same `outbox.sqlite` file â€” `idempotency_key` is UNIQUE and SQLite
//! serialises writers, so essential call/SMS records survive a Desktop
//! restart exactly as they do on the command path.
//!
//! The whole radio surface is Windows-only (WinUSB); on other targets
//! [`spawn`] returns an error and the plugin simply never has a radio.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
#[cfg(feature = "voice")]
use std::time::{Duration, Instant};

use aokie_core::events::DesktopEvent;
use serde_json::json;

use crate::config::PairedDevice;
use crate::event_bridge::{emit_event, Sink};
use crate::outbox::Outbox;

/// A control request from the main RPC thread to the radio thread. Every
/// variant is fire-and-forget: the *result* of the action arrives back as an
/// asynchronous `aokie.*` event from the radio thread, matching the mock
/// contract (`call.answer` â†’ later `aokie.call.answered`, etc.).
pub enum RadioControl {
    Answer,
    Reject,
    Hangup,
    SendSms {
        to: String,
        body: String,
    },
    /// Speak text to the caller. Stage 1 acknowledges + logs; the TTS â†’
    /// SCO audio path is wired in Stage 2.
    Speak {
        text: String,
    },
    /// Live-reconfigure the in-plugin voice agent without a reconnect. Each
    /// field is `Some` only when it changed; `None` leaves the current value
    /// alone. A flow (or `settings.set`) pushes this so the receptionist's
    /// persona/greeting/voice/model can be edited from FormLogic and take
    /// effect on the very next caller turn (or the next call's greeting).
    Configure {
        persona: Option<String>,
        greeting: Option<String>,
        voice: Option<String>,
        model: Option<String>,
        endpoint: Option<String>,
        stt_endpoint: Option<String>,
        tts_endpoint: Option<String>,
    },
    Shutdown,
}

/// Default receptionist system prompt when none is configured (voice build). A
/// goal-directed SCRIPT, not just a style: greet, get the caller's name and
/// reason, capture the key details, and book them in or take a message â€” one
/// short spoken question at a time. Editable live via the `persona` setting /
/// a flow push, so most deployments override this.
#[cfg(all(target_os = "windows", feature = "voice"))]
const DEFAULT_AGENT_PERSONA: &str = "You are Aokie, a warm, efficient phone receptionist for a small \
business, speaking out loud on a live phone call. If the caller asks who you are or your name, say \
you are Aokie, the automated receptionist - never invent a different name for yourself. Reply with ONE short, natural spoken sentence â€” no \
lists, markdown, or emoji. Your job: greet the caller, find out their name and how you can help, \
capture the key details (what they need, and a callback number or time if relevant), and either book \
them in or take a message. Ask only ONE clear question at a time and keep the conversation moving. \
IMPORTANT - only promise what actually happens: you take booking REQUESTS and messages for the team \
to confirm, so say things like I have noted that down and someone will confirm with you - NEVER say \
you will send a text, SMS, email, or confirmation yourself, and never claim something is booked, \
sent, or done, because you cannot send messages and bookings are confirmed by a person afterwards.";

/// Spoken on answer when no greeting is configured. A BLANK greeting setting means
/// "use the default", never "answer silently" — a desktop settings-form save (which
/// writes the full settings bag, greeting included) or a flow push with an empty form
/// field must not silence the receptionist. Shared by the spawn path (connector.rs)
/// and the live `RadioControl::Configure` path below.
pub const DEFAULT_GREETING: &str = "Hello, thanks for calling. How can I help you today?";

/// Live radio status, shared (via `Arc`) between the radio thread (writer)
/// and the main RPC thread (reader) so `phone.status` / `dongle.diagnostics`
/// answer without round-tripping the radio thread.
#[derive(Default)]
pub struct RadioStatus {
    pub initialized: AtomicBool,
    pub connected: AtomicBool,
    pub call_active: AtomicBool,
    pub local_address: Mutex<Option<String>>,
    pub connected_address: Mutex<Option<String>>,
    pub current_caller: Mutex<Option<String>>,
    /// The current call's id (the `call_…` correlation id every event for
    /// this call carries). Set the moment a call starts ringing, cleared on
    /// termination — this is what lets `call.current` recover a live call
    /// after a page refresh and lets call controls verify a `callId`.
    pub current_call_id: Mutex<Option<String>>,
    /// The settings revision in effect (audit AOK-CONFIG-002): stamped by the
    /// connector at spawn and on every settings.set, embedded in call.ended
    /// so a call record identifies exactly which configuration it ran under.
    pub config_version: AtomicU64,
    /// ISO-8601 instant the current call started RINGING (not answered) —
    /// the Live Call screen's timer runs from here.
    pub call_started_at: Mutex<Option<String>>,
    /// Devices seen connected during this radio session (the durable link
    /// keys live in the aokie pairing store; this is the live view).
    pub paired: Mutex<Vec<PairedDevice>>,
    /// Last fatal reason the radio reported (no dongle, driver not bound, â€¦).
    pub last_error: Mutex<Option<String>>,
    /// Speech results that arrived for a call that was no longer current and
    /// were DROPPED instead of being attributed to the wrong caller (audit
    /// C-05). Observable via dongle.diagnostics.
    pub stale_stt_results: AtomicU64,
}

/// Handle held by the [`Plugin`](crate::connector::Plugin): send control
/// requests and read live status. Dropping it (process shutdown) drops the
/// `control_tx`, which ends the radio loop and shuts the runtime down.
pub struct RadioHandle {
    control_tx: Sender<RadioControl>,
    pub status: Arc<RadioStatus>,
}

impl RadioHandle {
    pub fn send(&self, c: RadioControl) -> Result<(), String> {
        self.control_tx
            .send(c)
            .map_err(|_| "the radio thread is not running".to_string())
    }

    pub fn is_initialized(&self) -> bool {
        self.status.initialized.load(Ordering::Relaxed)
    }
    pub fn is_connected(&self) -> bool {
        self.status.connected.load(Ordering::Relaxed)
    }
    pub fn is_call_active(&self) -> bool {
        self.status.call_active.load(Ordering::Relaxed)
    }
    pub fn local_address(&self) -> Option<String> {
        self.status.local_address.lock().unwrap().clone()
    }
    pub fn connected_address(&self) -> Option<String> {
        self.status.connected_address.lock().unwrap().clone()
    }
    pub fn current_caller(&self) -> Option<String> {
        self.status.current_caller.lock().unwrap().clone()
    }
    pub fn current_call_id(&self) -> Option<String> {
        self.status.current_call_id.lock().unwrap().clone()
    }
    pub fn call_started_at(&self) -> Option<String> {
        self.status.call_started_at.lock().unwrap().clone()
    }
    pub fn paired(&self) -> Vec<PairedDevice> {
        self.status.paired.lock().unwrap().clone()
    }
    pub fn last_error(&self) -> Option<String> {
        self.status.last_error.lock().unwrap().clone()
    }
    pub fn stale_stt_results(&self) -> u64 {
        self.status.stale_stt_results.load(Ordering::Relaxed)
    }
}

/// The radio's outbox reference: the store plus HOW delivery is bookkept
/// (Legacy write-marks-sent vs ack-awaited — audit INT-003). Carried as one
/// value so every emit site stays a single `outbox` argument.
type OutboxRef<'a> = Option<(&'a Outbox, crate::event_bridge::EmitMode)>;

/// Emit one event best-effort: essential events route through the outbox
/// (write-before-emit) when it is open; otherwise fall back to a direct
/// stdout notification so a failed outbox never swallows a live call event.
fn emit(outbox: OutboxRef<'_>, sink: &mut dyn Sink, event: DesktopEvent) {
    match outbox {
        Some((o, mode)) => {
            if let Err(e) = emit_event(sink, o, &event, false, mode) {
                eprintln!("[aokie-plugin] radio emit '{}' failed: {e}", event.name);
            }
        }
        None => {
            let line = crate::rpc::notification_line("event.emit", json!({ "event": event }));
            let _ = sink.send_line(&line);
        }
    }
}

/// Emit an `aokie.call.turn.final` transcript turn, matching the contract the
/// Receptionist pack's app-logic + flow bindings expect: `{callId, turn,
/// speaker, text}` with a per-turn-unique idempotency key (`turn.<n>.final`) so
/// the app-logic dedup doesn't drop turns after the first. `speaker` is
/// "caller" (STT) or "bot" (Aokie's own speech); a flow gates its reply on
/// `speaker === 'caller'` so Aokie never answers itself.
#[cfg(feature = "voice")]
fn emit_turn(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    corr: &str,
    turn_index: u32,
    speaker: &str,
    text: &str,
) {
    emit(
        outbox,
        sink,
        aokie_core::events::aokie_turn_event(
            true,
            corr,
            turn_index,
            json!({
                "callId": corr,
                "turn": turn_index,
                "speaker": speaker,
                "text": text,
                "at": aokie_core::events::now_iso8601(),
            }),
        ),
    );
}

/// Emit the buffered `call.incoming` NOW if it hasn't gone out yet (audit
/// AOK-LIF-001). The incoming event is normally held briefly for caller-ID
/// enrichment; every other call-scoped event (ringing/answered/audio/ended)
/// forces it out first, so `incoming` ALWAYS precedes the rest of its call's
/// lifecycle regardless of the hold. `from` is whatever caller id has
/// arrived — empty when the phone hasn't sent one (never a sentinel, §8).
fn flush_incoming_if_pending(
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) {
    if !tracker.current().is_some_and(|s| s.incoming_pending()) {
        return;
    }
    let (corr, from) = {
        let s = tracker.current_mut().unwrap();
        s.mark_incoming_emitted();
        (s.id.clone(), s.caller_id.clone().unwrap_or_default())
    };
    emit(
        outbox,
        sink,
        aokie_core::events::aokie_event(
            crate::contract::events::CALL_INCOMING,
            &corr,
            json!({"callId": corr, "from": from, "at": aokie_core::events::now_iso8601()}),
        ),
    );
}

/// The canonical `call.ended` emission — shared by the phone's real
/// CallTerminated and the synthesized device-loss termination (audit
/// AOK-LIF-003), so both produce ONE identical terminal event shape.
/// The after-call flows key off this payload (contract §events): callId
/// mirrors the envelope correlation, from/callerPhone carry the caller id,
/// durationSeconds/durationMs count from ANSWER. The outcome comes from the
/// session state machine (audit AK-001): answered → "completed" (even a
/// sub-second call), operator-rejected → "rejected", never answered →
/// "missed", radio link gone → reason "device_lost".
fn emit_call_ended(
    ended: &crate::call_session::EndedCall,
    config_version: u64,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) {
    use aokie_core::events::{aokie_event, now_iso8601};
    let from = ended.caller_id.clone().unwrap_or_default();
    emit(
        outbox,
        sink,
        aokie_event(
            crate::contract::events::CALL_ENDED,
            &ended.id,
            json!({
                "at": now_iso8601(),
                "reason": ended.reason,
                "callId": ended.id,
                "from": from,
                "callerPhone": from,
                "durationSeconds": ended.duration_seconds,
                "durationMs": ended.duration_ms as u64,
                "outcome": ended.outcome,
                "configVersion": config_version,
            }),
        ),
    );
}

/// PII gate for conversation content in logs (audit PRIV-001/C-06): stderr is
/// captured by Desktop's log ring, so caller speech and agent replies appear
/// verbatim only when the operator explicitly opts in (`AOKIE_LOG_CONTENT=1`);
/// default logs carry only lengths.
#[cfg(feature = "voice")]
fn content_for_log(text: &str) -> String {
    if std::env::var("AOKIE_LOG_CONTENT").map(|v| v == "1").unwrap_or(false) {
        format!("{text:?}")
    } else {
        format!("[{} chars]", text.chars().count())
    }
}

/// Heuristic self-echo guard for the in-plugin agent: true when `caller` (a fresh
/// transcript) is mostly the same words as Aokie's last spoken reply `bot` â€” i.e.
/// Aokie's own TTS leaked back into the mic and STT transcribed it. Keeps Aokie
/// from answering itself if any audio escapes the half-duplex mute.
#[cfg(feature = "voice")]
fn looks_like_echo(caller: &str, bot: &str) -> bool {
    fn words(s: &str) -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_string())
            .collect()
    }
    let c = words(caller);
    if c.len() < 3 {
        return false; // too short to judge (e.g. "yes", "ok")
    }
    let b: std::collections::HashSet<String> = words(bot).into_iter().collect();
    if b.is_empty() {
        return false;
    }
    let overlap = c.iter().filter(|w| b.contains(*w)).count();
    (overlap as f32 / c.len() as f32) >= 0.7
}

/// How long a caller turn stays OPEN after a transcript that looks unfinished
/// (audit AK-008): callers read phone numbers in groups with pauses well past
/// the STT endpoint, and replying into that pause both talks over them and
/// books half a number. Long enough to bridge a between-groups breath, short
/// enough that a genuinely finished number only delays the reply by a beat.
#[cfg(feature = "voice")]
const CONTINUATION_HOLD: Duration = Duration::from_millis(1400);

/// Cap on a merged caller turn — past this, flush regardless (a runaway hold
/// must never buffer the whole call into one turn).
#[cfg(feature = "voice")]
const CONTINUATION_MAX_CHARS: usize = 240;

/// True when a transcript's tail says "the caller isn't done" (audit AK-008):
/// it ends in a digit group, a spoken number word, or a connective that
/// announces one ("my number is …"). Drives the continuation hold above.
#[cfg(feature = "voice")]
fn ends_with_unfinished_number(text: &str) -> bool {
    let Some(last) = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .last()
        .map(str::to_string)
    else {
        return false;
    };
    if last.chars().all(|c| c.is_ascii_digit()) {
        return true; // "…0412", "…345" — a digit group just ended at a pause
    }
    matches!(
        last.as_str(),
        // Spoken digits and digit multipliers…
        "zero" | "one" | "two" | "three" | "four" | "five" | "six" | "seven" | "eight"
            | "nine" | "oh" | "double" | "triple"
            // …and connectives that promise a number/detail is coming.
            | "is" | "its" | "on" | "number" | "and" | "um" | "uh"
    )
}

/// A caller turn being merged across STT utterances (audit AK-008). `corr` is
/// pinned at first fragment so a turn that outlives its call (hangup inside
/// the hold window) still lands against the right call record.
#[cfg(feature = "voice")]
struct PendingTurn {
    corr: String,
    text: String,
    flush_at: Instant,
}

// â”€â”€ Windows: the real radio â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Start the live radio on a background thread. Returns immediately with a
/// handle; initialisation happens asynchronously and is reflected in
/// [`RadioStatus`] (and via an `aokie.dongle.ready` / `aokie.hardware.error`
/// event). `preferred_path` pins a specific WinUSB dongle path, or `None`
/// takes the first enumerated HCI controller.
#[cfg(target_os = "windows")]
pub fn spawn(
    data_dir: std::path::PathBuf,
    preferred_path: Option<String>,
    auto_answer: bool,
    answer_tone: bool,
    reenumerate_hwid: Option<String>,
    greeting: Option<String>,
    ack_mode: bool,
) -> Result<RadioHandle, String> {
    use aokie_dongle::bluetooth::BluetoothManager;
    use std::sync::mpsc;

    let (control_tx, control_rx) = mpsc::channel::<RadioControl>();
    let status = Arc::new(RadioStatus::default());
    let status_thread = status.clone();

    std::thread::Builder::new()
        .name("aokie-plugin-radio".to_string())
        // Match the runtime thread's generous stack â€” the deep ACL â†’ L2CAP â†’
        // RFCOMM â†’ HFP dispatch overflowed the 1 MiB Windows default.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            // Software "virtual replug": on a cold boot the dongle's SCO iso
            // endpoint is dead until the device is re-enumerated (physically
            // unplug/replug). CM_Reenumerate the device before opening it so a
            // headless receptionist works after boot with no manual replug.
            // Best-effort + gated (settings.reenumerateHwid); settle briefly so
            // the device + WinUSB re-bind before we open it.
            if let Some(hwid) = reenumerate_hwid.as_deref() {
                match aokie_dongle::winusb::restart_device(hwid) {
                    Ok(()) => {
                        eprintln!("[aokie-plugin] restarted {hwid} (virtual replug: remove + re-add) â€” settling 3s");
                        std::thread::sleep(std::time::Duration::from_millis(3000));
                    }
                    Err(e) => eprintln!("[aokie-plugin] virtual replug {hwid} failed (continuing): {e}"),
                }
            }
            // Raise this process's timer resolution to 1 ms for the lifetime of
            // the radio (see Cargo.toml note). The SCO iso path services USB
            // frames every 1 ms; at the ~15.6 ms per-process default a bare
            // plugin's read_sco waits + TX pacing are too coarse and every iso
            // transfer fails (empty + Win32 87). The original Tauri app gets
            // this for free via WebView2. timeBeginPeriod is ref-counted and
            // paired with timeEndPeriod below.
            unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
            let mut bt = match BluetoothManager::new_with_preferred_dongle(preferred_path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[aokie-plugin] radio failed to start: {e}");
                    *status_thread.last_error.lock().unwrap() = Some(e);
                    unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
                    return;
                }
            };
            // Second connection to the same outbox file (see module docs).
            // Fail CLOSED (audit AOK-RUN-001): call/SMS events are the
            // business record — if they can't be durably queued, the radio
            // must not run and LOOK available while silently downgrading to
            // direct stdout. The error lands in last_error, so health reads
            // degraded and the operator sees why.
            let outbox = match Outbox::open(&data_dir.join(crate::connector::OUTBOX_FILE)) {
                Ok(o) => o,
                Err(e) => {
                    let msg = format!(
                        "radio outbox unavailable ({e}) — refusing to start without durable event delivery"
                    );
                    eprintln!("[aokie-plugin] {msg}");
                    *status_thread.last_error.lock().unwrap() = Some(msg);
                    unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
                    return;
                }
            };
            let mode = crate::event_bridge::EmitMode::from_ack(ack_mode);
            let mut sink = crate::event_bridge::StdoutSink::new();
            // Exit supervision (audit AOK-RUN-001): the spawner returned long
            // ago — if the loop panics or returns, readiness must flip
            // IMMEDIATELY, never leaving "initialized/connected" green on a
            // thread that no longer exists.
            let status_exit = status_thread.clone();
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_loop(
                    &mut bt,
                    Some((&outbox, mode)),
                    &mut sink,
                    control_rx,
                    status_thread,
                    auto_answer,
                    answer_tone,
                    greeting,
                );
            }));
            if ran.is_err() {
                let msg = "radio thread panicked — phone service stopped".to_string();
                eprintln!("[aokie-plugin] {msg}");
                *status_exit.last_error.lock().unwrap() = Some(msg);
            }
            status_exit.initialized.store(false, Ordering::Relaxed);
            status_exit.connected.store(false, Ordering::Relaxed);
            status_exit.call_active.store(false, Ordering::Relaxed);
            *status_exit.current_call_id.lock().unwrap() = None;
            unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
        })
        .map_err(|e| format!("spawn radio thread: {e}"))?;

    Ok(RadioHandle { control_tx, status })
}

/// A short two-note chime (mono i16 at the SCO sample rate) used to verify the
/// OUTBOUND SCO audio path actually reaches the caller on a given dongle â€” real
/// TTS speech replaces it once outbound audio is confirmed. Fades each note in
/// and out to avoid clicks.
#[cfg(target_os = "windows")]
fn greeting_tone(sample_rate: u16) -> Vec<i16> {
    let sr = sample_rate.max(8000) as f32;
    let mut out = Vec::new();
    for &(freq, secs) in &[(660.0f32, 0.35f32), (880.0, 0.5)] {
        let n = (sr * secs) as usize;
        for i in 0..n {
            let t = i as f32 / sr;
            let env = ((i as f32 / n as f32) * std::f32::consts::PI).sin(); // 0â†’1â†’0
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * env * 0.6;
            out.push((s * i16::MAX as f32) as i16);
        }
    }
    out
}

/// Result of speaking a phrase: how long the audio will play out, and whether
/// the caller barged in (started speaking) mid-phrase so we cut it short.
#[cfg(all(target_os = "windows", feature = "voice"))]
struct SpeakOutcome {
    dur: std::time::Duration,
    barged: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
struct HttpSpeechFallback {
    endpoint: Option<String>,
    failed_for_call: bool,
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
impl HttpSpeechFallback {
    fn new(endpoint: Option<String>) -> Self {
        Self {
            endpoint: normalize_endpoint(endpoint),
            failed_for_call: false,
        }
    }

    fn from_env(var: &str) -> Self {
        Self::new(std::env::var(var).ok())
    }

    fn configure(&mut self, endpoint: Option<String>) {
        let endpoint = normalize_endpoint(endpoint);
        if endpoint != self.endpoint {
            self.endpoint = endpoint;
            self.failed_for_call = false;
        }
    }

    fn reset_call(&mut self) {
        self.failed_for_call = false;
    }

    fn endpoint_for_call(&self) -> Option<&str> {
        if self.failed_for_call {
            None
        } else {
            self.endpoint.as_deref()
        }
    }

    fn mark_failed_for_call(&mut self) -> bool {
        if self.endpoint.is_some() && !self.failed_for_call {
            self.failed_for_call = true;
            true
        } else {
            false
        }
    }
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
fn normalize_endpoint(endpoint: Option<String>) -> Option<String> {
    endpoint
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(feature = "voice")]
enum SttWork {
    /// One finished caller utterance, stamped with the call it belongs to
    /// (audit C-05): the worker skips jobs whose generation is no longer
    /// current, and the consumer drops results the same way — a slow
    /// transcription from call A can never be attributed to call B.
    Utterance {
        generation: u64,
        utterance: u32,
        samples: Vec<f32>,
    },
    Configure {
        endpoint: Option<String>,
    },
    ResetCall,
}

/// A finished transcription, still carrying the identity of the call whose
/// audio produced it.
#[cfg(feature = "voice")]
struct SttResult {
    generation: u64,
    utterance: u32,
    text: String,
}

#[cfg(feature = "voice")]
struct HttpTtsRuntime {
    fallback: HttpSpeechFallback,
    client: reqwest::blocking::Client,
}

#[cfg(feature = "voice")]
impl HttpTtsRuntime {
    fn from_env(var: &str) -> Self {
        Self {
            fallback: HttpSpeechFallback::from_env(var),
            client: http_speech_client(),
        }
    }

    fn configure(&mut self, endpoint: Option<String>) {
        self.fallback.configure(endpoint);
    }

    fn reset_call(&mut self) {
        self.fallback.reset_call();
    }
}

#[cfg(feature = "voice")]
fn http_speech_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

#[cfg(feature = "voice")]
fn http_stt_transcribe(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    samples_16k: &[f32],
) -> Result<String, String> {
    let pcm: Vec<i16> = samples_16k
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let wav = crate::speech_wire::encode_wav_pcm16_mono(&pcm, 16_000);
    let audio = format!(
        "data:audio/wav;base64,{}",
        crate::speech_wire::base64_encode(&wav)
    );
    let resp = client
        .post(endpoint)
        .json(&serde_json::json!({ "audio": audio }))
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

#[cfg(feature = "voice")]
fn http_tts_synthesize(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    text: &str,
    voice: &str,
) -> Result<crate::speech_wire::WavPcm, String> {
    let resp = client
        .post(endpoint)
        .json(&serde_json::json!({ "input": text, "voice": voice }))
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
fn decode_tts_json_audio(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let v: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("tts http JSON response parse failed: {e}"))?;
    let audio = v
        .get("b64_json")
        .or_else(|| v.get("audio"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "tts http JSON response missing b64_json/audio".to_string())?;
    crate::speech_wire::base64_decode_text(audio)
}

/// Feed one captured mic chunk through the echo canceller and update the
/// sustained-speech counter, returning `true` once the caller has spoken over
/// Aokie for `need` frames straight. `armed` gates the counting so the adaptive
/// filter has time to converge at a reply's onset (we still call
/// `process_capture` while un-armed â€” to keep the reference FIFO aligned with
/// capture â€” but never trip). Shared by the synth callback and the playout monitor.
#[cfg(all(target_os = "windows", feature = "voice"))]
fn detect_barge(
    aec: &mut crate::aec::EchoCanceller,
    mic: &[i16],
    frame: usize,
    thr: f32,
    armed: bool,
    speech_frames: &mut u32,
    need: u32,
) -> bool {
    let cleaned = aec.process_capture(mic);
    if !armed {
        return false;
    }
    for f in cleaned.chunks(frame) {
        if crate::voice::frame_rms(f) > thr {
            *speech_frames += 1;
            if *speech_frames >= need {
                return true;
            }
        } else {
            *speech_frames = speech_frames.saturating_sub(1);
        }
    }
    false
}

#[cfg(all(target_os = "windows", feature = "voice"))]
struct TtsChunkPlayback {
    t0: std::time::Instant,
    t_first: std::time::Instant,
    first: bool,
    samples: usize,
    barged: bool,
    frame: usize,
    speech_frames: u32,
    need: u32,
    grace: std::time::Duration,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
impl TtsChunkPlayback {
    fn new(sample_rate: u16) -> Self {
        let t0 = std::time::Instant::now();
        Self {
            t0,
            t_first: t0,
            first: true,
            samples: 0,
            barged: false,
            frame: (sample_rate as usize / 100).max(80),
            speech_frames: 0,
            need: 22,
            grace: std::time::Duration::from_millis(350),
        }
    }

    fn push(
        &mut self,
        bt: &mut aokie_dongle::bluetooth::BluetoothManager,
        aec: &mut Option<&mut crate::aec::EchoCanceller>,
        barge_rms: Option<f32>,
        pcm: &[i16],
    ) -> bool {
        if pcm.is_empty() {
            return !self.barged;
        }
        if self.first {
            eprintln!(
                "[aokie-plugin] speaking (first audio in {:?})",
                self.t0.elapsed()
            );
            self.first = false;
            self.t_first = std::time::Instant::now();
        }
        if let Some(a) = aec.as_deref_mut() {
            a.feed_reference(pcm);
        }
        bt.send_audio(pcm);
        self.samples += pcm.len();

        // Full-duplex: drain + echo-cancel the mic as we feed (keeps the
        // reference FIFO aligned with capture) and watch for the caller talking
        // over us.
        if let (Some(a), Some(thr)) = (aec.as_deref_mut(), barge_rms) {
            let armed = self.t_first.elapsed() >= self.grace;
            while let Some(rx) = bt.try_recv_audio() {
                if detect_barge(
                    a,
                    &rx.samples,
                    self.frame,
                    thr,
                    armed,
                    &mut self.speech_frames,
                    self.need,
                ) {
                    self.barged = true;
                    break;
                }
            }
        }
        !self.barged
    }

    fn finish(
        mut self,
        bt: &mut aokie_dongle::bluetooth::BluetoothManager,
        aec: &mut Option<&mut crate::aec::EchoCanceller>,
        barge_rms: Option<f32>,
        text: &str,
        sample_rate: u16,
    ) -> SpeakOutcome {
        use std::time::Duration;

        // Playout monitor (full-duplex): synthesis can outrun realtime, so a
        // short reply may still be draining from the SCO queue after the chunks
        // have all been queued. Keep polling the mic through the AEC until it
        // has played out.
        if let (Some(a), Some(thr)) = (aec.as_deref_mut(), barge_rms) {
            if !self.barged {
                let playout =
                    Duration::from_secs_f32(self.samples as f32 / sample_rate.max(1) as f32);
                let deadline = self.t_first + playout;
                while std::time::Instant::now() < deadline {
                    let armed = self.t_first.elapsed() >= self.grace;
                    let mut got = false;
                    while let Some(rx) = bt.try_recv_audio() {
                        got = true;
                        if detect_barge(
                            a,
                            &rx.samples,
                            self.frame,
                            thr,
                            armed,
                            &mut self.speech_frames,
                            self.need,
                        ) {
                            self.barged = true;
                            break;
                        }
                    }
                    if self.barged {
                        break;
                    }
                    if !got {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }

        eprintln!(
            "[aokie-plugin] spoke ({} chars -> {} samples @ {}Hz, synth {:?}{})",
            text.chars().count(),
            self.samples,
            sample_rate,
            self.t0.elapsed(),
            if self.barged { ", BARGED-IN" } else { "" }
        );
        SpeakOutcome {
            dur: Duration::from_secs_f32(self.samples as f32 / sample_rate.max(1) as f32),
            barged: self.barged,
        }
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
fn http_tts_chunk_samples(sample_rate: u16) -> usize {
    (sample_rate as usize / 50).max(160)
}

/// Voice build only: synthesize + stream `text` to SCO with the in-process TTS
/// engine (loaded lazily on first use). When `aec`/`barge_rms` are set (full-
/// duplex mode) it feeds each played chunk as the echo reference, echo-cancels
/// the inbound mic, and watches for the caller starting to speak over Aokie â€”
/// both while synthesizing AND through the queued playout tail â€” returning
/// `barged: true` and stopping early if so. With them `None` it's the plain
/// half-duplex stream (caller relies on the mute). No-op with no SCO channel
/// (sample_rate 0) or empty text.
#[cfg(all(target_os = "windows", feature = "voice"))]
fn tts_speak(
    bt: &mut aokie_dongle::bluetooth::BluetoothManager,
    tts: &mut Option<crate::voice::TtsEngine>,
    http_tts: &mut HttpTtsRuntime,
    text: &str,
    sample_rate: u16,
    mut aec: Option<&mut crate::aec::EchoCanceller>,
    barge_rms: Option<f32>,
) -> SpeakOutcome {
    use std::time::Duration;
    let none = SpeakOutcome {
        dur: Duration::ZERO,
        barged: false,
    };
    if sample_rate == 0 || text.trim().is_empty() {
        return none;
    }
    // Speech-normalize ONCE at the chokepoint (greeting, agent sentences and
    // operatorSpeak all funnel through here): "10 a.m.," → "10 AM," — dotted
    // abbreviations against punctuation make the TTS stutter audibly.
    let text = &crate::speech_wire::normalize_speech_text(text);
    // Voice from AOKIE_TTS_VOICE, shared by HTTP and in-process synthesis.
    let voice = std::env::var("AOKIE_TTS_VOICE").unwrap_or_default();
    if let Some(endpoint) = http_tts.fallback.endpoint_for_call().map(str::to_string) {
        match http_tts_synthesize(&http_tts.client, &endpoint, text, &voice) {
            Ok(wav) => {
                let pcm = crate::speech_wire::resample_i16_mono(
                    &wav.samples,
                    wav.sample_rate,
                    sample_rate as u32,
                );
                let mut playback = TtsChunkPlayback::new(sample_rate);
                for chunk in pcm.chunks(http_tts_chunk_samples(sample_rate)) {
                    if !playback.push(bt, &mut aec, barge_rms, chunk) {
                        break;
                    }
                }
                return playback.finish(bt, &mut aec, barge_rms, text, sample_rate);
            }
            Err(e) => {
                if http_tts.fallback.mark_failed_for_call() {
                    eprintln!(
                        "[aokie-plugin] HTTP TTS failed at {endpoint}: {e}; falling back to in-process TTS for this call"
                    );
                }
            }
        }
    }
    if tts.is_none() {
        match crate::voice::TtsEngine::load() {
            Ok(e) => {
                eprintln!("[aokie-plugin] TTS engine loaded");
                *tts = Some(e);
            }
            Err(e) => {
                eprintln!("[aokie-plugin] TTS load failed: {e}");
                return none;
            }
        }
    }
    let engine = match tts.as_mut() {
        Some(e) => e,
        None => return none,
    };
    // Stream each chunk to the SCO queue as synthesized so the caller hears the
    // reply start on the first chunk (~0.3s).
    let mut playback = TtsChunkPlayback::new(sample_rate);
    let synth = engine.synthesize_streaming(text, &voice, sample_rate as u32, |pcm| {
        playback.push(bt, &mut aec, barge_rms, pcm)
    });
    if let Err(e) = synth {
        eprintln!("[aokie-plugin] TTS synthesis failed: {e}");
        return none;
    }
    playback.finish(bt, &mut aec, barge_rms, text, sample_rate)
}

/// The radio poll loop: drain events â†’ map+emit; buffer the incoming-call
/// emission until the caller id lands (or a short timeout); drain audio
/// (Stage 2 feeds the AI here); service control requests. Runs until the
/// control channel closes or a Shutdown is received.
#[cfg(target_os = "windows")]
// `greeting` is only mutated (via RadioControl::Configure) in the voice build.
#[cfg_attr(not(feature = "voice"), allow(unused_mut))]
fn run_loop(
    bt: &mut aokie_dongle::bluetooth::BluetoothManager,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: std::sync::mpsc::Receiver<RadioControl>,
    status: Arc<RadioStatus>,
    auto_answer: bool,
    answer_tone: bool,
    mut greeting: Option<String>,
) {
    use std::sync::mpsc::TryRecvError;
    use std::time::Duration;
    #[cfg(feature = "voice")]
    use std::time::Instant;

    // Lazily-loaded in-process TTS (voice build only). Loaded on the first thing
    // Aokie needs to say (greeting or operatorSpeak) so a call with no speech
    // never pays the ~200 MB model-load cost.
    #[cfg(feature = "voice")]
    let mut tts: Option<crate::voice::TtsEngine> = None;
    #[cfg(feature = "voice")]
    let mut http_tts = HttpTtsRuntime::from_env("AOKIE_TTS_ENDPOINT");
    let _ = &greeting; // used only in the voice build / greeting block below

    // â”€â”€ Speech-to-text (voice build) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // The caller's audio is transcribed OFF the radio loop: a worker thread owns
    // the heavy Parakeet engine (lazy-loaded on the first utterance) so a ~300 ms
    // transcription never stalls SCO I/O or control handling. The loop segments
    // utterances with a simple energy VAD and ships each finished one to the
    // worker; finished transcripts come back and become `aokie.call.turn.final`
    // events â€” the hook a flow binds to drive the conversation.
    // The CURRENT call generation, shared with the worker: jobs stamped with
    // any other generation are for a finished call — the worker skips them
    // WITHOUT transcribing (cheap cancellation on hangup; audit AK-002).
    #[cfg(feature = "voice")]
    let stt_current_gen = Arc::new(AtomicU64::new(0));
    #[cfg(feature = "voice")]
    let (stt_tx, stt_result_rx) = {
        let (utter_tx, utter_rx) = std::sync::mpsc::channel::<SttWork>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<SttResult>();
        let initial_stt_endpoint = std::env::var("AOKIE_STT_ENDPOINT").ok();
        let worker_gen = stt_current_gen.clone();
        std::thread::Builder::new()
            .name("aokie-stt".into())
            .spawn(move || {
                let mut engine: Option<crate::voice::SttEngine> = None;
                let client = http_speech_client();
                let mut http_stt = HttpSpeechFallback::new(initial_stt_endpoint);
                while let Ok(work) = utter_rx.recv() {
                    let (generation, utterance, buf) = match work {
                        SttWork::Utterance {
                            generation,
                            utterance,
                            samples,
                        } => (generation, utterance, samples),
                        SttWork::Configure { endpoint } => {
                            http_stt.configure(endpoint);
                            continue;
                        }
                        SttWork::ResetCall => {
                            http_stt.reset_call();
                            continue;
                        }
                    };
                    // Stale-job gate: the call this audio belongs to is over
                    // (or a new one replaced it) — do not spend a transcription
                    // on it, and never emit its text.
                    if generation != worker_gen.load(Ordering::Relaxed) {
                        eprintln!(
                            "[aokie-plugin] skipped stale STT job (call gen {generation}, utterance {utterance})"
                        );
                        continue;
                    }
                    let send = |text: String| {
                        let _ = res_tx.send(SttResult {
                            generation,
                            utterance,
                            text,
                        });
                    };
                    if let Some(endpoint) = http_stt.endpoint_for_call().map(str::to_string) {
                        match http_stt_transcribe(&client, &endpoint, &buf) {
                            Ok(text) if !text.is_empty() => {
                                send(text);
                                continue;
                            }
                            Ok(_) => continue,
                            Err(e) => {
                                if http_stt.mark_failed_for_call() {
                                    eprintln!(
                                        "[aokie-plugin] HTTP STT failed at {endpoint}: {e}; falling back to in-process STT for this call"
                                    );
                                }
                            }
                        }
                    }
                    if engine.is_none() {
                        match crate::voice::SttEngine::load() {
                            Ok(e) => {
                                eprintln!("[aokie-plugin] STT engine loaded");
                                engine = Some(e);
                            }
                            Err(e) => {
                                eprintln!("[aokie-plugin] STT load failed: {e}");
                                continue;
                            }
                        }
                    }
                    if let Some(eng) = engine.as_mut() {
                        match eng.transcribe(&buf) {
                            Ok(text) if !text.is_empty() => send(text),
                            Ok(_) => {}
                            Err(e) => eprintln!("[aokie-plugin] STT transcribe failed: {e}"),
                        }
                    }
                }
            })
            .ok();
        (utter_tx, res_rx)
    };
    // VAD / utterance accumulator (all in 16 kHz mono f32, the STT engine's rate).
    #[cfg(feature = "voice")]
    let mut stt_buf: Vec<f32> = Vec::new();
    // Utterances sent to the STT worker whose results haven't come back yet
    // (audit AOK-LIF-002): the termination drain waits for these — bounded —
    // so the caller's last words land BEFORE call.ended.
    let mut stt_outstanding: usize = 0;
    #[cfg(feature = "voice")]
    let mut stt_had_speech = false;
    #[cfg(feature = "voice")]
    let mut stt_silence = std::time::Duration::ZERO;
    // End-of-utterance silence: how long the caller must pause before we treat
    // their turn as finished and transcribe. Lower = snappier replies but risks
    // cutting off mid-sentence pauses. Tunable via AOKIE_STT_ENDPOINT_MS (set from
    // the `sttEndpointMs` connector setting); default 450 ms.
    #[cfg(feature = "voice")]
    let stt_endpoint = std::time::Duration::from_millis(
        std::env::var("AOKIE_STT_ENDPOINT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&m| (150..=2000).contains(&m))
            .unwrap_or(450),
    );

    // â”€â”€ In-plugin real-time voice agent â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // When AOKIE_AI_RECEPTIONIST is set (from the `aiReceptionist` setting), the
    // plugin answers the caller ITSELF â€” streaming the local LLM (reused from the
    // desktop's llama.cpp/ollama) and speaking each sentence as it's generated â€”
    // instead of routing through a flow. Far lower latency. The pack's live-reply
    // flow binding must be disabled so the caller isn't answered twice.
    #[cfg(feature = "voice")]
    let agent_enabled = std::env::var_os("AOKIE_AI_RECEPTIONIST").is_some();
    #[cfg(feature = "voice")]
    let mut agent_endpoint = std::env::var("AOKIE_AI_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty());
    // System prompt / script (AOKIE_AI_PERSONA from the `persona` setting, or a flow
    // push). Editable live via RadioControl::Configure. The default is a real
    // receptionist SCRIPT â€” greet, get the caller's name + reason, capture details,
    // book or take a message â€” not just a chat style, so it actively drives the call.
    #[cfg(feature = "voice")]
    let mut agent_persona = std::env::var("AOKIE_AI_PERSONA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AGENT_PERSONA.to_string());
    // LLM model for the agent (AOKIE_AI_MODEL from `aiModel`, or a flow push). Empty
    // = auto-detect whatever the desktop's running LLM has loaded.
    #[cfg(feature = "voice")]
    let mut agent_model = std::env::var("AOKIE_AI_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    #[cfg(feature = "voice")]
    let mut agent_client: Option<crate::agent::LlmClient> = None;
    // Conversation history for the agent (OpenAI chat messages), reset per call.
    #[cfg(feature = "voice")]
    let mut history: Vec<serde_json::Value> = Vec::new();
    // Caller turn held open across STT utterances (audit AK-008 — see
    // PendingTurn): replies wait until the turn stops looking unfinished.
    #[cfg(feature = "voice")]
    let mut pending_turn: Option<PendingTurn> = None;
    // Controls that arrived DURING an agent reply (audit AK-003): the
    // mid-reply poll acts on Hangup/Reject instantly and parks everything
    // else here; the main control loop drains this before its channel.
    let mut pending_controls: std::collections::VecDeque<RadioControl> = Default::default();
    // Aokie's last spoken line (greeting or reply) â€” for the self-echo guard.
    #[cfg(feature = "voice")]
    let mut last_bot_reply = String::new();
    // Half-duplex gate: while Aokie is speaking (+ a short tail) inbound audio is
    // discarded so we never transcribe our own TTS echoing back over the line.
    #[cfg(feature = "voice")]
    let mut mute_stt_until: Option<std::time::Instant> = None;
    // â”€â”€ Full-duplex / barge-in â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // When AOKIE_BARGE_IN is set (the `bargeIn` setting) Aokie keeps LISTENING
    // while it speaks: outbound TTS is echo-cancelled from the inbound mic so the
    // caller can talk over the receptionist, which stops as soon as they do. Off
    // by default â†’ the proven half-duplex mute path above stays the norm. Only
    // meaningful with the agent on (it drives the interruptible reply loop).
    #[cfg(feature = "voice")]
    let barge_in = agent_enabled && std::env::var_os("AOKIE_BARGE_IN").is_some();
    // Cleaned-mic RMS above which the caller counts as speaking over Aokie. Set
    // above the AEC's residual echo floor; tune per handset via AOKIE_BARGE_RMS.
    #[cfg(feature = "voice")]
    let barge_rms: f32 = std::env::var("AOKIE_BARGE_RMS")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|&v| v > 0.0)
        .unwrap_or(650.0);
    // The echo canceller, built lazily once we know the negotiated SCO rate.
    #[cfg(feature = "voice")]
    let mut aec: Option<crate::aec::EchoCanceller> = None;
    // Monotonic transcript turn index (caller + bot share one sequence), reset
    // per call. 1-based to match the simulated-call convention (`turn.1.final`,
    // `turn.2.final`, â€¦) so real + simulated calls dedup + display identically.
    #[cfg(feature = "voice")]
    let mut turn_index: u32 = 1;

    // Per-call state: ONE explicit session state machine (audit AK-001) owns
    // the call id, generation, phase, timing, caller id, termination intent
    // and the once-per-call flags (incoming emitted / auto-answered / toned /
    // greeted) that used to be six scattered Option<String>s here.
    //
    // Auto-answer fires IMMEDIATELY when a call appears â€” NOT after a ring
    // delay â€” because on some dongles the SCO/audio channel that comes up
    // right after the ring blocks the radio's main loop, so a late answer
    // never gets serviced. Answering in the brief pre-SCO window is what
    // gets the AT+ATA out. (Emitting `aokie.call.incoming` still waits for
    // the caller id; only the answer is hurried.)
    let mut tracker = crate::call_session::SessionTracker::new();
    // Which generation the voice pipeline is configured for; a change (new
    // call OR idle) resets per-call voice state and re-stamps the STT gate.
    #[cfg(feature = "voice")]
    let mut voice_call_gen: u64 = 0;

    loop {
        let mut idle = true;

        while let Some(ev) = bt.try_recv_event() {
            idle = false;
            // Final-transcript drain (audit AOK-LIF-002): the caller's last
            // words must land BEFORE call.ended — summaries and after-call
            // flows key off ended, and the last sentence is often the most
            // important one. On termination of a live call: finalize any
            // buffered audio as the closing utterance, wait (bounded) for
            // in-flight STT, and flush the held turn — THEN let handle_event
            // publish the terminal event. The call is already over, so the
            // short stall cannot delay answering it.
            #[cfg(feature = "voice")]
            if matches!(ev, aokie_dongle::bluetooth::BluetoothEvent::CallTerminated)
                && tracker.current().is_some()
            {
                if stt_had_speech && stt_buf.len() >= 16_000 / 3 {
                    if let Some(s) = tracker.current_mut() {
                        let utterance = s.next_utterance_id();
                        if stt_tx
                            .send(SttWork::Utterance {
                                generation: s.generation,
                                utterance,
                                samples: std::mem::take(&mut stt_buf),
                            })
                            .is_ok()
                        {
                            stt_outstanding += 1;
                        }
                    }
                }
                stt_buf.clear();
                stt_had_speech = false;
                stt_silence = Duration::ZERO;

                let gen_now = tracker.generation();
                let deadline = Instant::now() + Duration::from_millis(1500);
                while stt_outstanding > 0 {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        eprintln!(
                            "[aokie-plugin] final-transcript drain timed out with {stt_outstanding} STT job(s) in flight"
                        );
                        break;
                    }
                    match stt_result_rx.recv_timeout(left) {
                        Ok(SttResult { generation, text, .. }) => {
                            stt_outstanding = stt_outstanding.saturating_sub(1);
                            if generation != gen_now {
                                status.stale_stt_results.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            if agent_enabled && looks_like_echo(&text, &last_bot_reply) {
                                continue;
                            }
                            match pending_turn.as_mut() {
                                Some(p) => {
                                    p.text.push(' ');
                                    p.text.push_str(text.trim());
                                }
                                None => {
                                    pending_turn = Some(PendingTurn {
                                        corr: tracker.call_id().unwrap_or_default().to_string(),
                                        text: text.trim().to_string(),
                                        flush_at: Instant::now(),
                                    })
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                if let Some(p) = pending_turn.take() {
                    if !p.corr.is_empty() && !p.text.is_empty() {
                        emit_turn(outbox, sink, &p.corr, turn_index, "caller", &p.text);
                        turn_index += 1;
                    }
                }
            }
            handle_event(ev, &mut tracker, outbox, sink, &status);
        }

        // Per-call voice isolation (audit AK-002/C-05): the moment the session
        // generation changes — a call ended, or a new one replaced it — stamp
        // the STT gate (queued jobs for the old generation are skipped, their
        // results dropped) and reset EVERY piece of per-call voice state:
        // conversation history, transcript turn index, utterance buffers, echo
        // canceller and mute gate. Doing it on generation change (not on an
        // observed idle tick) means back-to-back calls in one event batch can
        // never leak history or half-built utterances into the next call.
        #[cfg(feature = "voice")]
        if voice_call_gen != tracker.generation() {
            // A turn still held open when its call ends (hangup inside the
            // continuation window) is RECORDED against that call — losing the
            // caller's last fragment (often the tail of a phone number) is
            // worse than a late turn event — but never answered.
            if let Some(p) = pending_turn.take() {
                if !p.corr.is_empty() && !p.text.is_empty() {
                    eprintln!(
                        "[aokie-plugin] flushing held caller turn from ended call: {}",
                        content_for_log(&p.text)
                    );
                    emit_turn(outbox, sink, &p.corr, turn_index, "caller", &p.text);
                }
            }
            voice_call_gen = tracker.generation();
            stt_current_gen.store(voice_call_gen, Ordering::Relaxed);
            http_tts.reset_call();
            let _ = stt_tx.send(SttWork::ResetCall);
            // Self-healing (AOK-LIF-002): skipped/empty STT jobs never send a
            // result, so the in-flight counter resets at every call boundary
            // rather than accumulating drift across calls.
            stt_outstanding = 0;
            turn_index = 1;
            history.clear();
            last_bot_reply.clear();
            stt_buf.clear();
            stt_had_speech = false;
            stt_silence = Duration::ZERO;
            mute_stt_until = None;
            if let Some(a) = aec.as_mut() {
                a.reset();
            }
        }

        // Flush a buffered incoming call once the caller id is known or the
        // grace window elapses.
        let flush_incoming = tracker.current().is_some_and(|s| {
            s.incoming_pending() && (s.caller_id.is_some() || s.incoming_pending_ms() > 800)
        });
        if flush_incoming {
            flush_incoming_if_pending(&mut tracker, outbox, sink);
        }

        // Auto-answer ASAP: the instant a call is present and not yet answered,
        // send the answer â€” before the audio channel comes up and freezes the
        // loop. Answer exactly once per call (the session's auto_answered flag).
        if auto_answer {
            if let Some(s) = tracker.current_mut() {
                if !s.auto_answered && !s.is_active() {
                    match bt.answer_call() {
                        Ok(()) => {
                            eprintln!("[aokie-plugin] auto-answered incoming call (immediate)")
                        }
                        Err(e) => eprintln!("[aokie-plugin] auto-answer failed: {e}"),
                    }
                    s.auto_answered = true;
                    idle = false;
                }
            }
        }

        // Stage-2 diagnostic: once the call's audio channel is up (sample rate
        // becomes non-zero), play a short two-note chime to the caller to verify
        // the OUTBOUND SCO path actually reaches the phone on this dongle. Real
        // TTS speech replaces this once outbound audio is confirmed. Gated by
        // settings.answerTone.
        if answer_tone {
            let sr = bt.get_sample_rate();
            if let Some(s) = tracker.current_mut() {
                if !s.toned && sr > 0 {
                    let tone = greeting_tone(sr);
                    eprintln!(
                        "[aokie-plugin] answerTone: sending {} samples @ {}Hz to the caller",
                        tone.len(),
                        sr
                    );
                    bt.send_audio(&tone);
                    s.toned = true;
                    idle = false;
                }
            }
        }

        // Greet the caller with real TTS speech once the SCO audio channel is up
        // (voice build). Plays exactly once per call (the session's `greeted`
        // flag); per-call voice state RESETS live in the generation block above.
        let greet_now = {
            let sr = bt.get_sample_rate();
            match tracker.current_mut() {
                Some(s) if !s.greeted && sr > 0 => {
                    s.greeted = true;
                    Some((s.id.clone(), sr))
                }
                _ => None,
            }
        };
        if let Some((corr, sr)) = greet_now {
            #[cfg(not(feature = "voice"))]
            let _ = (&corr, sr);
            #[cfg(feature = "voice")]
            {
                // Build the echo canceller once we know the negotiated SCO
                // rate (full-duplex only). Reused for every phrase this call.
                if barge_in && aec.is_none() {
                    aec = Some(crate::aec::EchoCanceller::new(sr as u32));
                    eprintln!(
                        "[aokie-plugin] full-duplex barge-in ON (AEC @ {sr}Hz, rms>{barge_rms})"
                    );
                }
                if let Some(text) = greeting.as_deref() {
                    // In barge-in mode the caller can talk over the greeting;
                    // in half-duplex we mute STT for its playout instead.
                    let (aec_ref, brms) = if barge_in {
                        (aec.as_mut(), Some(barge_rms))
                    } else {
                        (None, None)
                    };
                    let out = tts_speak(bt, &mut tts, &mut http_tts, text, sr, aec_ref, brms);
                    if barge_in {
                        if out.barged {
                            bt.flush_tx_audio();
                        }
                        mute_stt_until = None;
                    } else {
                        mute_stt_until =
                            Some(Instant::now() + out.dur + Duration::from_millis(400));
                    }
                    stt_buf.clear();
                    stt_had_speech = false;
                    stt_silence = Duration::ZERO;
                    emit_turn(outbox, sink, &corr, turn_index, "bot", text);
                    turn_index += 1;
                    history.push(serde_json::json!({ "role": "assistant", "content": text }));
                    last_bot_reply = text.to_string();
                }
            }
            idle = false;
        }

        // Inbound caller audio.
        #[cfg(not(feature = "voice"))]
        while bt.try_recv_audio().is_some() {
            idle = false;
        }
        // Voice build: energy-VAD segment the caller's speech â†’ ship each finished
        // utterance to the STT worker. ~350 RMS (i16 units) gates speech; ~700 ms
        // of trailing silence ends an utterance; sub-350 ms blips are dropped.
        #[cfg(feature = "voice")]
        {
            const SPEECH_RMS: f32 = 350.0;
            let endpoint = stt_endpoint;
            let muted = mute_stt_until.is_some_and(|t| Instant::now() < t);
            while let Some(frame) = bt.try_recv_audio() {
                idle = false;
                if tracker.current().is_none() {
                    continue;
                }
                // Full-duplex: echo-cancel the mic (so Aokie's own voice, even
                // when it's mid-reply, doesn't transcribe as the caller) and keep
                // reference consumption 1:1 with the mic. Half-duplex: the mute
                // window swallows the echo instead, so we just skip while muted.
                let samples: std::borrow::Cow<[i16]> = if barge_in {
                    match aec.as_mut() {
                        Some(a) => {
                            let cleaned = a.process_capture(&frame.samples);
                            if cleaned.is_empty() {
                                continue;
                            }
                            std::borrow::Cow::Owned(cleaned)
                        }
                        None => std::borrow::Cow::Borrowed(&frame.samples[..]),
                    }
                } else {
                    if muted {
                        continue;
                    }
                    std::borrow::Cow::Borrowed(&frame.samples[..])
                };
                let rms = crate::voice::frame_rms(&samples);
                let f16 = crate::voice::to_f32_16k(&samples, frame.sample_rate as u32);
                let frame_dur =
                    Duration::from_secs_f32(samples.len() as f32 / frame.sample_rate.max(1) as f32);
                if rms > SPEECH_RMS {
                    stt_had_speech = true;
                    stt_silence = Duration::ZERO;
                    stt_buf.extend_from_slice(&f16);
                } else if stt_had_speech {
                    stt_silence += frame_dur;
                    stt_buf.extend_from_slice(&f16); // keep trailing silence for context
                }
                if stt_buf.len() > 16_000 * 15 {
                    stt_silence = endpoint; // force-flush a runaway (~15 s) utterance
                }
            }
            if stt_had_speech && stt_silence >= endpoint {
                if stt_buf.len() >= 16_000 / 3 {
                    // Stamp the job with the call it belongs to (audit C-05).
                    if let Some(s) = tracker.current_mut() {
                        let utterance = s.next_utterance_id();
                        if stt_tx
                            .send(SttWork::Utterance {
                                generation: s.generation,
                                utterance,
                                samples: std::mem::take(&mut stt_buf),
                            })
                            .is_ok()
                        {
                            stt_outstanding += 1;
                        }
                    } else {
                        stt_buf.clear();
                    }
                } else {
                    stt_buf.clear();
                }
                stt_had_speech = false;
                stt_silence = Duration::ZERO;
            }
            // Finished transcripts: accumulate into the OPEN caller turn
            // (audit AK-008). A transcript whose tail looks unfinished — a
            // digit group mid-phone-number, "my number is…" — keeps the turn
            // open for CONTINUATION_HOLD instead of triggering a reply into
            // the caller's pause; the next utterance merges into it. Anything
            // else flushes on the spot (no added latency for normal turns).
            while let Ok(SttResult {
                generation,
                utterance,
                text,
            }) = stt_result_rx.try_recv()
            {
                idle = false;
                stt_outstanding = stt_outstanding.saturating_sub(1);
                // Stale-result gate (audit C-05): only text whose generation IS
                // the current call may be recorded or answered. A slow result
                // from a previous call is dropped and counted — never spoken
                // to, or attributed to, the next caller.
                if tracker.current().map(|s| s.generation) != Some(generation) {
                    let n = status.stale_stt_results.fetch_add(1, Ordering::Relaxed) + 1;
                    eprintln!(
                        "[aokie-plugin] DROPPED stale STT result (call gen {generation}, utterance {utterance}, current gen {}, {n} total): {}",
                        tracker.generation(),
                        content_for_log(&text)
                    );
                    continue;
                }
                // Drop a transcript that's really Aokie's own reply echoing back
                // (belt-and-suspenders over the half-duplex mute) so it never
                // records it as a caller turn or answers itself.
                if agent_enabled && looks_like_echo(&text, &last_bot_reply) {
                    eprintln!("[aokie-plugin] ignored self-echo: {}", content_for_log(&text));
                    continue;
                }
                let corr = tracker.call_id().unwrap_or_default().to_string();
                match pending_turn.as_mut() {
                    Some(p) => {
                        p.text.push(' ');
                        p.text.push_str(text.trim());
                        p.corr = corr;
                    }
                    None => {
                        pending_turn = Some(PendingTurn {
                            corr,
                            text: text.trim().to_string(),
                            flush_at: Instant::now(),
                        })
                    }
                }
                let p = pending_turn.as_mut().expect("just set");
                if p.text.len() < CONTINUATION_MAX_CHARS && ends_with_unfinished_number(&p.text) {
                    p.flush_at = Instant::now() + CONTINUATION_HOLD;
                    eprintln!(
                        "[aokie-plugin] holding turn open (looks unfinished): {}",
                        content_for_log(&p.text)
                    );
                } else {
                    p.flush_at = Instant::now();
                }
            }
            // Flush the open turn once its hold expired AND the caller isn't
            // mid-utterance (fresh speech extends the merge window naturally).
            let flushed_turn = match pending_turn.as_ref() {
                Some(p) if Instant::now() >= p.flush_at && !stt_had_speech => {
                    pending_turn.take().map(|p| (p.corr, p.text))
                }
                _ => None,
            };
            if let Some((corr, text)) = flushed_turn {
                idle = false;
                {
                    eprintln!("[aokie-plugin] heard [turn {turn_index}]: {}", content_for_log(&text));
                    emit_turn(outbox, sink, &corr, turn_index, "caller", &text);
                    turn_index += 1;

                    if agent_enabled {
                        history.push(serde_json::json!({ "role": "user", "content": text }));
                        if history.len() > 24 {
                            let drop = history.len() - 24;
                            history.drain(..drop);
                        }
                        // Lazily connect to the local LLM on the first caller turn.
                        if agent_client.is_none() {
                            match crate::agent::discover_endpoint(agent_endpoint.as_deref()) {
                                Some(ep) => {
                                    let c = crate::agent::LlmClient::new(ep, agent_model.clone());
                                    eprintln!(
                                        "[aokie-plugin] voice agent LLM: {} (model {:?})",
                                        c.endpoint(),
                                        c.model()
                                    );
                                    agent_client = Some(c);
                                }
                                None => eprintln!(
                                    "[aokie-plugin] voice agent: no local LLM reachable (:8080/:11434)"
                                ),
                            }
                        }
                        if let Some(client) = agent_client.as_ref() {
                            let sr = bt.get_sample_rate();
                            let mut messages = vec![
                                serde_json::json!({ "role": "system", "content": agent_persona }),
                            ];
                            messages.extend(history.iter().cloned());
                            // Half-duplex: mute STT for the WHOLE reply as it streams.
                            // Sentences synthesize faster than they play, so the audio
                            // keeps playing (queued) after synthesis finishes; muting
                            // only the last sentence let the tail echo back and Aokie
                            // answered itself. Track cumulative playback from t0.
                            // Full-duplex (barge_in): no mute â€” the AEC keeps the mic
                            // clean AND watches for the caller talking over the reply,
                            // cutting it short (flush the queued tail) the moment they do.
                            let t0 = Instant::now();
                            let mut reply_dur = Duration::ZERO;
                            let mut barged = false;
                            // What the caller actually HEARD (audit AK-008):
                            // sentences that reached the speaker, including the
                            // one cut mid-play by a barge-in. The history/turn
                            // record uses this, never the full generation.
                            let mut spoken: Vec<String> = Vec::new();
                            eprintln!("[aokie-plugin] agent replying (streaming)â€¦");
                            let outcome =
                                client.stream_reply(serde_json::json!(messages), |sentence| {
                                    // Audit AK-003: the radio loop is inside this
                                    // stream — without this poll a Hangup waits for
                                    // the WHOLE reply. Hangup/Reject act right here
                                    // (worst-case latency: one sentence); everything
                                    // else is parked for the main control loop.
                                    while let Ok(ctl) = control_rx.try_recv() {
                                        match ctl {
                                            RadioControl::Hangup => {
                                                tracker.note_intent(
                                                    crate::call_session::TerminationIntent::OperatorHangup,
                                                );
                                                bt.flush_tx_audio();
                                                if let Err(e) = bt.hangup() {
                                                    eprintln!("[aokie-plugin] mid-reply hangup failed: {e}");
                                                }
                                                barged = true; // record only what played
                                                return false; // abort the reply now
                                            }
                                            RadioControl::Reject => {
                                                tracker.note_intent(
                                                    crate::call_session::TerminationIntent::OperatorReject,
                                                );
                                                bt.flush_tx_audio();
                                                if let Err(e) = bt.reject_call() {
                                                    eprintln!("[aokie-plugin] mid-reply reject failed: {e}");
                                                }
                                                barged = true;
                                                return false;
                                            }
                                            other => pending_controls.push_back(other),
                                        }
                                    }
                                    eprintln!(
                                        "[aokie-plugin] agent sentence (+{:?}): {}",
                                        t0.elapsed(),
                                        content_for_log(sentence)
                                    );
                                    if barge_in {
                                        let out = tts_speak(
                                            bt,
                                            &mut tts,
                                            &mut http_tts,
                                            sentence,
                                            sr,
                                            aec.as_mut(),
                                            Some(barge_rms),
                                        );
                                        reply_dur += out.dur;
                                        spoken.push(sentence.trim().to_string());
                                        if out.barged {
                                            bt.flush_tx_audio(); // stop the queued tail now
                                            barged = true;
                                            return false; // stop pulling from the LLM
                                        }
                                    } else {
                                        let out = tts_speak(
                                            bt,
                                            &mut tts,
                                            &mut http_tts,
                                            sentence,
                                            sr,
                                            None,
                                            None,
                                        );
                                        reply_dur += out.dur;
                                        let plays_until = (t0 + reply_dur).max(Instant::now());
                                        mute_stt_until =
                                            Some(plays_until + Duration::from_millis(600));
                                    }
                                    true
                                });
                            if !barge_in {
                                // Cover audio still queued after the last chunk synthesized.
                                let plays_until = (t0 + reply_dur).max(Instant::now());
                                mute_stt_until = Some(plays_until + Duration::from_millis(800));
                            }
                            match outcome {
                                Ok(full) => {
                                    // Truthful transcript (audit AK-008): a barged
                                    // reply records what actually PLAYED — the full
                                    // generation includes sentences the caller never
                                    // heard, and letting the LLM "remember" saying
                                    // them corrupts every turn after the interrupt.
                                    let heard = if barged {
                                        let h = spoken.join(" ").trim().to_string();
                                        if h.is_empty() { h } else { format!("{h} [caller interrupted]") }
                                    } else {
                                        full.trim().to_string()
                                    };
                                    if !heard.is_empty() {
                                        history.push(
                                            serde_json::json!({ "role": "assistant", "content": heard }),
                                        );
                                        emit_turn(outbox, sink, &corr, turn_index, "bot", &heard);
                                        turn_index += 1;
                                        last_bot_reply = heard;
                                    }
                                    if !barged {
                                        // Discard anything captured while we replied.
                                        // On barge-in, KEEP it: the caller's interrupting
                                        // speech is already accumulating as their next turn.
                                        stt_buf.clear();
                                        stt_had_speech = false;
                                        stt_silence = Duration::ZERO;
                                    } else {
                                        eprintln!(
                                            "[aokie-plugin] caller barged in â€” reply cut short"
                                        );
                                    }
                                }
                                Err(e) => eprintln!("[aokie-plugin] agent reply failed: {e}"),
                            }
                        }
                    }
                }
            }
        }

        loop {
            // Controls deferred by the mid-reply poll (audit AK-003) run first,
            // in arrival order, before anything newly queued.
            let next = match pending_controls.pop_front() {
                Some(c) => Ok(c),
                None => control_rx.try_recv(),
            };
            match next {
                Ok(RadioControl::Answer) => {
                    if let Err(e) = bt.answer_call() {
                        eprintln!("[aokie-plugin] radio answer failed: {e}");
                    }
                }
                Ok(RadioControl::Reject) => {
                    // Record WHY before the phone acts, so the eventual
                    // CallTerminated reads outcome "rejected", never "missed"
                    // (audit AK-001/AK-01).
                    tracker.note_intent(crate::call_session::TerminationIntent::OperatorReject);
                    if let Err(e) = bt.reject_call() {
                        eprintln!("[aokie-plugin] radio reject failed: {e}");
                    }
                }
                Ok(RadioControl::Hangup) => {
                    tracker.note_intent(crate::call_session::TerminationIntent::OperatorHangup);
                    if let Err(e) = bt.hangup() {
                        eprintln!("[aokie-plugin] radio hangup failed: {e}");
                    }
                }
                Ok(RadioControl::SendSms { to, body }) => {
                    if let Err(e) = bt.send_sms(to, body, None) {
                        emit(
                            outbox,
                            sink,
                            aokie_core::events::aokie_event(
                                crate::contract::events::HARDWARE_ERROR,
                                "radio",
                                json!({"message": format!("send_sms failed: {e}")}),
                            ),
                        );
                    }
                }
                Ok(RadioControl::Speak { text }) => {
                    #[cfg(feature = "voice")]
                    if agent_enabled {
                        // The in-plugin agent owns the conversation, so ignore any
                        // operatorSpeak the flow still emits (its binding may be a
                        // stale enabled-copy in the desktop's runtime cache) â€”
                        // otherwise the caller is answered twice.
                        eprintln!(
                            "[aokie-plugin] ignoring operatorSpeak (agent owns replies): {}",
                            content_for_log(&text)
                        );
                    } else {
                        let sr = bt.get_sample_rate();
                        let out = tts_speak(bt, &mut tts, &mut http_tts, &text, sr, None, None);
                        mute_stt_until =
                            Some(Instant::now() + out.dur + Duration::from_millis(400));
                        stt_buf.clear();
                        stt_had_speech = false;
                        stt_silence = Duration::ZERO;
                        // Truthful transcript (audit AOK-VOICE-002): record a
                        // bot turn ONLY when synthesis actually produced audio
                        // for the caller. A zero-duration outcome means TTS
                        // failed — the transcript must not claim speech the
                        // caller never heard.
                        if out.dur > Duration::ZERO {
                            if let Some(corr) = tracker.call_id().map(str::to_string) {
                                emit_turn(outbox, sink, &corr, turn_index, "bot", &text);
                                turn_index += 1;
                            }
                        } else {
                            eprintln!(
                                "[aokie-plugin] operatorSpeak produced NO audio (TTS failed) — not recorded as a spoken turn: {}",
                                content_for_log(&text)
                            );
                        }
                    }
                    #[cfg(not(feature = "voice"))]
                    eprintln!(
                        "[aokie-plugin] operatorSpeak ({} chars) â€” voice feature not built",
                        text.chars().count()
                    );
                }
                Ok(RadioControl::Configure {
                    persona,
                    greeting: g,
                    voice,
                    model,
                    endpoint,
                    stt_endpoint,
                    tts_endpoint,
                }) => {
                    // Live-reconfigure the agent from a flow / settings.set push. Each
                    // field is Some only when it changed. Greeting applies to the NEXT
                    // call; persona/voice/model take effect on the next caller turn.
                    #[cfg(feature = "voice")]
                    {
                        if let Some(g) = g {
                            // Blank = default, never silence (see DEFAULT_GREETING).
                            greeting = if g.trim().is_empty() {
                                Some(DEFAULT_GREETING.to_string())
                            } else {
                                Some(g)
                            };
                        }
                        if let Some(p) = persona {
                            if !p.trim().is_empty() {
                                agent_persona = p;
                            }
                        }
                        if let Some(v) = voice {
                            std::env::set_var("AOKIE_TTS_VOICE", v.trim());
                        }
                        let mut client_stale = false;
                        if let Some(m) = model {
                            let m = m.trim().to_string();
                            let new = if m.is_empty() { None } else { Some(m) };
                            if new != agent_model {
                                agent_model = new;
                                client_stale = true;
                            }
                        }
                        if let Some(e) = endpoint {
                            let e = e.trim().to_string();
                            let new = if e.is_empty() { None } else { Some(e) };
                            if new != agent_endpoint {
                                agent_endpoint = new;
                                client_stale = true;
                            }
                        }
                        if client_stale {
                            // Force a reconnect with the new endpoint/model next turn.
                            agent_client = None;
                        }
                        if let Some(e) = stt_endpoint {
                            let _ = stt_tx.send(SttWork::Configure { endpoint: Some(e) });
                        }
                        if let Some(e) = tts_endpoint {
                            http_tts.configure(Some(e));
                        }
                        eprintln!("[aokie-plugin] agent reconfigured (persona/greeting/voice/model/endpoints)");
                    }
                    #[cfg(not(feature = "voice"))]
                    {
                        let _ = (
                            persona,
                            g,
                            voice,
                            model,
                            endpoint,
                            stt_endpoint,
                            tts_endpoint,
                        );
                    }
                }
                Ok(RadioControl::Shutdown) => return,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
            idle = false;
        }

        if idle {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

/// Map one `BluetoothEvent` to the `aokie.*` contract: drive the call-session
/// state machine (audit AK-001) + mirror shared status. Available on all
/// targets (it only touches `BluetoothEvent`, which is not Windows-gated) so
/// the whole lifecycle stays unit-testable.
fn handle_event(
    ev: aokie_dongle::bluetooth::BluetoothEvent,
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
) {
    use aokie_core::events::{aokie_event, now_iso8601};
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    match ev {
        E::Initialized(addr) => {
            status.initialized.store(true, Ordering::Relaxed);
            *status.local_address.lock().unwrap() = Some(addr.clone());
            *status.last_error.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::DONGLE_READY,
                    "radio",
                    json!({"address": addr, "source": "radio"}),
                ),
            );
        }
        E::DeviceConnected(addr) => {
            status.connected.store(true, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = Some(addr.clone());
            {
                let mut paired = status.paired.lock().unwrap();
                if !paired.iter().any(|d| d.address == addr) {
                    paired.push(PairedDevice {
                        address: addr.clone(),
                        name: "Paired phone".to_string(),
                    });
                }
            }
            emit(
                outbox,
                sink,
                aokie_event(crate::contract::events::PHONE_CONNECTED, "radio", json!({"address": addr})),
            );
        }
        E::DeviceDisconnected(addr) => {
            // A live session cannot outlive its radio link (audit
            // AOK-LIF-003): synthesize the terminal outcome NOW — otherwise
            // call.current and the operator UI stay "live" on hardware that
            // is gone. A late real CallTerminated lands on an idle tracker
            // and is a no-op (no duplicate terminal event).
            if tracker.current().is_some() {
                flush_incoming_if_pending(tracker, outbox, sink);
                status.call_active.store(false, Ordering::Relaxed);
                tracker.note_intent(crate::call_session::TerminationIntent::DeviceLost);
                if let Some(ended) = tracker.terminate() {
                    eprintln!(
                        "[aokie-plugin] phone link lost during call {} — synthesized termination (outcome {})",
                        ended.id, ended.outcome
                    );
                    emit_call_ended(&ended, status.config_version.load(Ordering::Relaxed), outbox, sink);
                }
                *status.current_caller.lock().unwrap() = None;
                *status.current_call_id.lock().unwrap() = None;
                *status.call_started_at.lock().unwrap() = None;
            }
            status.connected.store(false, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::PHONE_DISCONNECTED,
                    "radio",
                    json!({"address": addr}),
                ),
            );
        }
        E::CallIncoming => {
            // Some phones emit the ring / callsetup indicator more than once
            // for a single call — the tracker starts a NEW session only when
            // idle, so incoming + greeting fire exactly once per call.
            let id = format!("call_{}", uuid::Uuid::new_v4().simple());
            if let Some(s) = tracker.ring(id, now_iso8601()) {
                // Shared call identity: `call.current` recovers a live call
                // from these after a browser refresh (audit C-02), and call
                // controls verify their `callId` against it (audit C-01).
                *status.current_caller.lock().unwrap() = None;
                *status.current_call_id.lock().unwrap() = Some(s.id.clone());
                *status.call_started_at.lock().unwrap() = Some(s.started_at_iso.clone());
            }
        }
        E::CallerId(num) => {
            tracker.caller_id(num.clone());
            *status.current_caller.lock().unwrap() = Some(num);
        }
        E::CallRinging => {
            flush_incoming_if_pending(tracker, outbox, sink);
            if let Some(corr) = tracker.call_id() {
                emit(
                    outbox,
                    sink,
                    aokie_event(
                        crate::contract::events::CALL_RINGING,
                        corr,
                        json!({"at": now_iso8601()}),
                    ),
                );
            }
        }
        E::CallAnswered => {
            // Lifecycle order (audit AOK-LIF-001): incoming ALWAYS precedes
            // answered — even when the caller-ID hold hasn't elapsed yet.
            flush_incoming_if_pending(tracker, outbox, sink);
            status.call_active.store(true, Ordering::Relaxed);
            tracker.answered();
            if let Some(corr) = tracker.call_id() {
                emit(
                    outbox,
                    sink,
                    aokie_event(
                        crate::contract::events::CALL_ANSWERED,
                        corr,
                        json!({"at": now_iso8601()}),
                    ),
                );
            }
        }
        E::CallTerminated => {
            // Even an instantly-abandoned ring gets its incoming record
            // before the terminal event (audit AOK-LIF-001).
            flush_incoming_if_pending(tracker, outbox, sink);
            status.call_active.store(false, Ordering::Relaxed);
            if let Some(ended) = tracker.terminate() {
                emit_call_ended(&ended, status.config_version.load(Ordering::Relaxed), outbox, sink);
            }
            *status.current_caller.lock().unwrap() = None;
            *status.current_call_id.lock().unwrap() = None;
            *status.call_started_at.lock().unwrap() = None;
        }
        E::AudioConnected { codec, sample_rate } => {
            flush_incoming_if_pending(tracker, outbox, sink);
            let corr = tracker.call_id().unwrap_or("radio").to_string();
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::CALL_AUDIO_CONNECTED,
                    &corr,
                    json!({"codec": codec, "sampleRate": sample_rate}),
                ),
            );
        }
        E::AudioDisconnected => {
            flush_incoming_if_pending(tracker, outbox, sink);
            let corr = tracker.call_id().unwrap_or("radio").to_string();
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::CALL_AUDIO_DISCONNECTED,
                    &corr,
                    json!({}),
                ),
            );
        }
        E::SmsReceived(p) => {
            let corr = format!("sms_{}", uuid::Uuid::new_v4().simple());
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::SMS_RECEIVED,
                    &corr,
                    json!({
                        "from": p.sender_phone,
                        "name": p.sender_name,
                        "body": p.body,
                        "handle": p.handle,
                        "at": now_iso8601(),
                    }),
                ),
            );
        }
        E::SmsSent { recipient_phone } => {
            let message_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::SMS_SENT,
                    &message_id,
                    json!({"messageId": message_id, "to": recipient_phone, "at": now_iso8601()}),
                ),
            );
        }
        E::ContactsFetched(_) | E::MapNotificationsSubscribed => {
            // Phonebook / MNS-subscribe are diagnostic; nothing to surface yet.
        }
        E::Error(e) => {
            *status.last_error.lock().unwrap() = Some(e.clone());
            emit(
                outbox,
                sink,
                aokie_event(crate::contract::events::HARDWARE_ERROR, "radio", json!({"message": e})),
            );
        }
    }
}

// â”€â”€ Non-Windows: no radio â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[cfg(not(target_os = "windows"))]
pub fn spawn(
    _data_dir: std::path::PathBuf,
    _preferred_path: Option<String>,
    _auto_answer: bool,
    _answer_tone: bool,
    _reenumerate_hwid: Option<String>,
    _greeting: Option<String>,
    _ack_mode: bool,
) -> Result<RadioHandle, String> {
    Err("the Aokie radio is only supported on Windows (WinUSB)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit AK-008: the continuation heuristic must hold a turn open exactly
    /// when the caller sounds mid-number — digit groups, spoken digits, and
    /// the connectives that announce one — and never for a finished sentence.
    #[cfg(feature = "voice")]
    #[test]
    fn unfinished_number_heuristic() {
        // The live failure: a phone number read in groups with pauses.
        assert!(ends_with_unfinished_number("my number is 0412"));
        assert!(ends_with_unfinished_number("it's 0412 345"));
        assert!(ends_with_unfinished_number("zero four one two"));
        assert!(ends_with_unfinished_number("you can reach me on 0412, 345"));
        assert!(ends_with_unfinished_number("double four"));
        assert!(ends_with_unfinished_number("my number is"));
        assert!(ends_with_unfinished_number("you can call me on"));
        // Finished turns must flush immediately — no added latency.
        assert!(!ends_with_unfinished_number("I'd like to book a haircut"));
        assert!(!ends_with_unfinished_number("yes that's right"));
        assert!(!ends_with_unfinished_number("my name is Lance"));
        assert!(!ends_with_unfinished_number(""));
        assert!(!ends_with_unfinished_number("   "));
        // A word after the digits releases the hold.
        assert!(!ends_with_unfinished_number("nine thirty tomorrow"));
        assert!(!ends_with_unfinished_number("0412 345 678 thanks"));
    }

    /// Audit C-01/C-02/AK-001: the radio publishes the current call's identity
    /// (`callId` + `startedAt`) into the shared status the moment it rings
    /// and clears it on termination — `call.current` and the call-control
    /// `callId` guard read exactly these fields — and the session state
    /// machine decides the ended outcome.
    #[test]
    fn handle_event_tracks_shared_call_identity() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        macro_rules! apply {
            ($ev:expr) => {
                handle_event($ev, &mut tracker, None, &mut sink, &status)
            };
        }

        assert!(status.current_call_id.lock().unwrap().is_none());

        apply!(E::CallIncoming);
        let call_id = status.current_call_id.lock().unwrap().clone();
        assert_eq!(
            call_id.as_deref(),
            tracker.call_id(),
            "shared id mirrors the session"
        );
        assert!(call_id.as_deref().unwrap().starts_with("call_"));
        assert!(status.call_started_at.lock().unwrap().is_some());
        assert!(
            !status.call_active.load(Ordering::Relaxed),
            "ringing, not active"
        );
        let first_gen = tracker.generation();
        assert_eq!(first_gen, 1);

        // Phones re-emit the ring indicator — the id must not change mid-call.
        apply!(E::CallIncoming);
        assert_eq!(*status.current_call_id.lock().unwrap(), call_id);
        assert_eq!(tracker.generation(), first_gen);

        apply!(E::CallerId("+61400000001".to_string()));
        assert_eq!(
            status.current_caller.lock().unwrap().as_deref(),
            Some("+61400000001")
        );

        apply!(E::CallAnswered);
        assert!(status.call_active.load(Ordering::Relaxed));
        assert_eq!(*status.current_call_id.lock().unwrap(), call_id);

        sink.lines.clear();
        apply!(E::CallTerminated);
        assert!(status.current_call_id.lock().unwrap().is_none());
        assert!(status.call_started_at.lock().unwrap().is_none());
        assert!(status.current_caller.lock().unwrap().is_none());
        assert!(!status.call_active.load(Ordering::Relaxed));
        assert_eq!(tracker.generation(), 0, "idle after termination");
        // The ended event carried the SAME call id the whole call used, and
        // an ANSWERED call is completed even when it lasted under a second.
        let v: serde_json::Value = serde_json::from_str(&sink.lines[0]).unwrap();
        assert_eq!(
            v["params"]["event"]["name"],
            json!(crate::contract::events::CALL_ENDED)
        );
        assert_eq!(v["params"]["event"]["data"]["callId"], json!(call_id));
        assert_eq!(v["params"]["event"]["data"]["outcome"], json!("completed"));
        assert_eq!(v["params"]["event"]["data"]["from"], json!("+61400000001"));

        // The next call gets a FRESH id and a FRESH generation.
        apply!(E::CallIncoming);
        let second = status.current_call_id.lock().unwrap().clone();
        assert!(second.is_some());
        assert_ne!(second, call_id);
        assert_eq!(tracker.generation(), 2);
    }

    /// Audit AK-001/AK-01: an operator-rejected ring ends "rejected", a
    /// remote-abandoned ring ends "missed" — the two are no longer conflated.
    #[test]
    fn handle_event_rejected_is_not_missed() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        // Operator rejects the ringing call (RadioControl::Reject notes the
        // intent, then the phone reports termination).
        // CallTerminated force-flushes the pending incoming first (AOK-LIF-001),
        // so locate the terminal event by NAME, not position.
        let ended_event = |sink: &VecSink| -> serde_json::Value {
            sink.lines
                .iter()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .find(|v| v["params"]["event"]["name"] == json!(crate::contract::events::CALL_ENDED))
                .expect("a call.ended event")
        };
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        tracker.note_intent(crate::call_session::TerminationIntent::OperatorReject);
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        let v = ended_event(&sink);
        assert_eq!(v["params"]["event"]["data"]["outcome"], json!("rejected"));
        assert_eq!(
            v["params"]["event"]["data"]["reason"],
            json!("operator_reject")
        );

        // Remote abandons the next ring: genuinely missed.
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        let v = ended_event(&sink);
        assert_eq!(v["params"]["event"]["data"]["outcome"], json!("missed"));
    }

    /// Audit AOK-LIF-001: `incoming` always precedes the rest of its call's
    /// lifecycle. The caller-ID enrichment hold must not let an instant
    /// answer (or termination) overtake the canonical start-of-call event.
    #[test]
    fn handle_event_incoming_always_precedes_answered() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        // The phone answers within the enrichment hold — no run_loop flush
        // tick has happened between the two events.
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);

        let names: Vec<String> = sink
            .lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["params"]["event"]["name"].as_str().unwrap_or("").to_string()
            })
            .collect();
        let incoming_at = names.iter().position(|n| n == crate::contract::events::CALL_INCOMING);
        let answered_at = names.iter().position(|n| n == crate::contract::events::CALL_ANSWERED);
        assert!(incoming_at.is_some(), "incoming must be emitted (forced flush)");
        assert!(
            incoming_at < answered_at,
            "incoming must precede answered, got order {names:?}"
        );

        // An instantly-abandoned ring still gets incoming before ended.
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        let names: Vec<String> = sink
            .lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["params"]["event"]["name"].as_str().unwrap_or("").to_string()
            })
            .collect();
        let incoming_at = names.iter().position(|n| n == crate::contract::events::CALL_INCOMING);
        let ended_at = names.iter().position(|n| n == crate::contract::events::CALL_ENDED);
        assert!(incoming_at.is_some() && incoming_at < ended_at, "incoming precedes ended, got {names:?}");
    }

    /// Audit AOK-LIF-003: losing the phone/radio link under a live call
    /// synthesizes exactly ONE terminal `call.ended` (reason device_lost),
    /// clears the shared call identity, and a late real CallTerminated is a
    /// no-op — the UI can never stay "live" on hardware that is gone.
    #[test]
    fn handle_event_device_loss_terminates_the_active_call_once() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
        assert!(status.call_active.load(Ordering::Relaxed));
        sink.lines.clear();

        handle_event(
            E::DeviceDisconnected("AA:BB:CC:DD:EE:FF".into()),
            &mut tracker,
            None,
            &mut sink,
            &status,
        );

        let events: Vec<serde_json::Value> = sink
            .lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let ended: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| v["params"]["event"]["name"] == json!(crate::contract::events::CALL_ENDED))
            .collect();
        assert_eq!(ended.len(), 1, "exactly one terminal event");
        assert_eq!(ended[0]["params"]["event"]["data"]["reason"], json!("device_lost"));
        assert_eq!(ended[0]["params"]["event"]["data"]["outcome"], json!("completed"));
        assert!(!status.call_active.load(Ordering::Relaxed));
        assert!(status.current_call_id.lock().unwrap().is_none(), "call identity cleared");
        assert!(tracker.current().is_none(), "session consumed");

        // The phone reports the (now stale) termination later: no duplicate.
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        assert!(
            sink.lines.iter().all(|l| !l.contains(crate::contract::events::CALL_ENDED)),
            "late real termination after synthesized one is a no-op"
        );
    }

    #[test]
    fn http_speech_fallback_is_sticky_per_call() {
        let mut state = HttpSpeechFallback::new(Some(
            "  http://127.0.0.1:17920/v1/audio/speech  ".to_string(),
        ));
        assert_eq!(
            state.endpoint_for_call(),
            Some("http://127.0.0.1:17920/v1/audio/speech")
        );

        assert!(state.mark_failed_for_call());
        assert_eq!(state.endpoint_for_call(), None);
        assert!(!state.mark_failed_for_call());

        state.reset_call();
        assert_eq!(
            state.endpoint_for_call(),
            Some("http://127.0.0.1:17920/v1/audio/speech")
        );

        assert!(state.mark_failed_for_call());
        state.configure(Some("http://127.0.0.1:17920/v1/audio/speech2".to_string()));
        assert_eq!(
            state.endpoint_for_call(),
            Some("http://127.0.0.1:17920/v1/audio/speech2")
        );

        state.configure(Some("   ".to_string()));
        assert_eq!(state.endpoint_for_call(), None);
    }
}
