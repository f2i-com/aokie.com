//! Live Bluetooth radio integration.
//!
//! Runs the real `aokie_radio` `AokieRuntime` (via
//! [`aokie_dongle::bluetooth::BluetoothManager`]) on a dedicated background
//! thread, maps its `BluetoothEvent`s onto the `aokie.*` Desktop-event
//! contract (emitted straight to stdout **and** the durable outbox), and
//! accepts control requests — answer / reject / hangup / send-SMS / speak —
//! from the main RPC thread over an mpsc channel.
//!
//! ## Why a second `Outbox` + `StdoutSink` on this thread
//! The plugin's main loop blocks on stdin, so an asynchronous call event (a
//! call can ring at any instant) must be delivered without waiting for the
//! next RPC. `StdoutSink` writes one whole line under the stdout lock
//! (line-atomic), so this thread's sink and the main thread's sink never
//! interleave. The [`Outbox`] here is a *second* SQLite connection to the
//! same `outbox.sqlite` file — `idempotency_key` is UNIQUE and SQLite
//! serialises writers, so essential call/SMS records survive a Desktop
//! restart exactly as they do on the command path.
//!
//! The whole radio surface is Windows-only (WinUSB); on other targets
//! [`spawn`] returns an error and the plugin simply never has a radio.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use aokie_core::events::DesktopEvent;
use serde_json::json;

use crate::config::PairedDevice;
use crate::event_bridge::{emit_event, Sink};
use crate::outbox::Outbox;

/// A control request from the main RPC thread to the radio thread. Every
/// variant is fire-and-forget: the *result* of the action arrives back as an
/// asynchronous `aokie.*` event from the radio thread, matching the mock
/// contract (`call.answer` → later `aokie.call.answered`, etc.).
pub enum RadioControl {
    Answer,
    Reject,
    Hangup,
    SendSms { to: String, body: String },
    /// Speak text to the caller. Stage 1 acknowledges + logs; the TTS →
    /// SCO audio path is wired in Stage 2.
    Speak { text: String },
    Shutdown,
}

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
    /// Devices seen connected during this radio session (the durable link
    /// keys live in the aokie pairing store; this is the live view).
    pub paired: Mutex<Vec<PairedDevice>>,
    /// Last fatal reason the radio reported (no dongle, driver not bound, …).
    pub last_error: Mutex<Option<String>>,
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
    pub fn paired(&self) -> Vec<PairedDevice> {
        self.status.paired.lock().unwrap().clone()
    }
    pub fn last_error(&self) -> Option<String> {
        self.status.last_error.lock().unwrap().clone()
    }
}

/// Emit one event best-effort: essential events route through the outbox
/// (write-before-emit) when it is open; otherwise fall back to a direct
/// stdout notification so a failed outbox never swallows a live call event.
fn emit(outbox: Option<&Outbox>, sink: &mut dyn Sink, event: DesktopEvent) {
    match outbox {
        Some(o) => {
            if let Err(e) = emit_event(sink, o, &event, false) {
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
    outbox: Option<&Outbox>,
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

/// Heuristic self-echo guard for the in-plugin agent: true when `caller` (a fresh
/// transcript) is mostly the same words as Aokie's last spoken reply `bot` — i.e.
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

// ── Windows: the real radio ──────────────────────────────────────────────

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
) -> Result<RadioHandle, String> {
    use aokie_dongle::bluetooth::BluetoothManager;
    use std::sync::mpsc;

    let (control_tx, control_rx) = mpsc::channel::<RadioControl>();
    let status = Arc::new(RadioStatus::default());
    let status_thread = status.clone();

    std::thread::Builder::new()
        .name("aokie-plugin-radio".to_string())
        // Match the runtime thread's generous stack — the deep ACL → L2CAP →
        // RFCOMM → HFP dispatch overflowed the 1 MiB Windows default.
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
                        eprintln!("[aokie-plugin] restarted {hwid} (virtual replug: remove + re-add) — settling 3s");
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
            let outbox = Outbox::open(&data_dir.join(crate::connector::OUTBOX_FILE)).ok();
            if outbox.is_none() {
                eprintln!("[aokie-plugin] radio: outbox unavailable, emitting without durability");
            }
            let mut sink = crate::event_bridge::StdoutSink::new();
            run_loop(&mut bt, outbox.as_ref(), &mut sink, control_rx, status_thread, auto_answer, answer_tone, greeting);
            unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
        })
        .map_err(|e| format!("spawn radio thread: {e}"))?;

    Ok(RadioHandle { control_tx, status })
}

/// A short two-note chime (mono i16 at the SCO sample rate) used to verify the
/// OUTBOUND SCO audio path actually reaches the caller on a given dongle — real
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
            let env = ((i as f32 / n as f32) * std::f32::consts::PI).sin(); // 0→1→0
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * env * 0.6;
            out.push((s * i16::MAX as f32) as i16);
        }
    }
    out
}

/// Voice build only: synthesize `text` with the in-process TTS engine (loaded
/// lazily on first use) and play it to the caller via `send_audio`. No-op when
/// no SCO channel is up (sample_rate 0) or the text is empty.
#[cfg(all(target_os = "windows", feature = "voice"))]
/// Synthesize + play `text`, returning how long the audio will take to play out
/// (so the caller can hold STT muted for that window — half-duplex, no barge-in).
/// Returns `Duration::ZERO` if nothing was played.
fn tts_speak(
    bt: &aokie_dongle::bluetooth::BluetoothManager,
    tts: &mut Option<crate::voice::TtsEngine>,
    text: &str,
    sample_rate: u16,
) -> std::time::Duration {
    use std::time::Duration;
    if sample_rate == 0 || text.trim().is_empty() {
        return Duration::ZERO;
    }
    if tts.is_none() {
        match crate::voice::TtsEngine::load() {
            Ok(e) => {
                eprintln!("[aokie-plugin] TTS engine loaded");
                *tts = Some(e);
            }
            Err(e) => {
                eprintln!("[aokie-plugin] TTS load failed: {e}");
                return Duration::ZERO;
            }
        }
    }
    if let Some(engine) = tts.as_mut() {
        // Stream each chunk straight to the SCO queue as it's synthesized, so the
        // caller hears the reply start on the first chunk (~0.3 s) instead of after
        // the whole utterance is synthesized (~1-2 s) — the big perceived-latency win.
        // Voice from AOKIE_TTS_VOICE (ttsVoice setting); empty = bundle default.
        let voice = std::env::var("AOKIE_TTS_VOICE").unwrap_or_default();
        let t0 = std::time::Instant::now();
        let mut first = true;
        let mut samples = 0usize;
        match engine.synthesize_streaming(text, &voice, sample_rate as u32, |pcm| {
            if first {
                eprintln!("[aokie-plugin] speaking (first audio in {:?})", t0.elapsed());
                first = false;
            }
            bt.send_audio(pcm);
            samples += pcm.len();
            true
        }) {
            Ok(_) => {
                eprintln!(
                    "[aokie-plugin] spoke ({} chars → {} samples @ {}Hz, total synth {:?})",
                    text.chars().count(),
                    samples,
                    sample_rate,
                    t0.elapsed()
                );
                return Duration::from_secs_f32(samples as f32 / sample_rate.max(1) as f32);
            }
            Err(e) => eprintln!("[aokie-plugin] TTS synthesis failed: {e}"),
        }
    }
    Duration::ZERO
}

/// The radio poll loop: drain events → map+emit; buffer the incoming-call
/// emission until the caller id lands (or a short timeout); drain audio
/// (Stage 2 feeds the AI here); service control requests. Runs until the
/// control channel closes or a Shutdown is received.
#[cfg(target_os = "windows")]
fn run_loop(
    bt: &mut aokie_dongle::bluetooth::BluetoothManager,
    outbox: Option<&Outbox>,
    sink: &mut dyn Sink,
    control_rx: std::sync::mpsc::Receiver<RadioControl>,
    status: Arc<RadioStatus>,
    auto_answer: bool,
    answer_tone: bool,
    greeting: Option<String>,
) {
    use std::sync::mpsc::TryRecvError;
    use std::time::{Duration, Instant};

    // Lazily-loaded in-process TTS (voice build only). Loaded on the first thing
    // Aokie needs to say (greeting or operatorSpeak) so a call with no speech
    // never pays the ~200 MB model-load cost.
    #[cfg(feature = "voice")]
    let mut tts: Option<crate::voice::TtsEngine> = None;
    // Which call we've already greeted, so the greeting plays exactly once.
    let mut greeted_corr: Option<String> = None;
    let _ = &greeting; // used only in the voice build / greeting block below

    // ── Speech-to-text (voice build) ──────────────────────────────────────────
    // The caller's audio is transcribed OFF the radio loop: a worker thread owns
    // the heavy Parakeet engine (lazy-loaded on the first utterance) so a ~300 ms
    // transcription never stalls SCO I/O or control handling. The loop segments
    // utterances with a simple energy VAD and ships each finished one to the
    // worker; finished transcripts come back and become `aokie.call.turn.final`
    // events — the hook a flow binds to drive the conversation.
    #[cfg(feature = "voice")]
    let (stt_tx, stt_result_rx) = {
        let (utter_tx, utter_rx) = std::sync::mpsc::channel::<Vec<f32>>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("aokie-stt".into())
            .spawn(move || {
                let mut engine: Option<crate::voice::SttEngine> = None;
                while let Ok(buf) = utter_rx.recv() {
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
                            Ok(text) if !text.is_empty() => {
                                let _ = res_tx.send(text);
                            }
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

    // ── In-plugin real-time voice agent ───────────────────────────────────────
    // When AOKIE_AI_RECEPTIONIST is set (from the `aiReceptionist` setting), the
    // plugin answers the caller ITSELF — streaming the local LLM (reused from the
    // desktop's llama.cpp/ollama) and speaking each sentence as it's generated —
    // instead of routing through a flow. Far lower latency. The pack's live-reply
    // flow binding must be disabled so the caller isn't answered twice.
    #[cfg(feature = "voice")]
    let agent_enabled = std::env::var_os("AOKIE_AI_RECEPTIONIST").is_some();
    #[cfg(feature = "voice")]
    let agent_endpoint = std::env::var("AOKIE_AI_ENDPOINT").ok().filter(|s| !s.trim().is_empty());
    #[cfg(feature = "voice")]
    let agent_persona = std::env::var("AOKIE_AI_PERSONA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            "You are a warm, efficient phone receptionist for a small business. You are \
             speaking out loud on a live phone call, so reply with ONE short, natural spoken \
             sentence — no lists, no markdown, no emoji. If you need information, ask a single \
             clear question."
                .to_string()
        });
    #[cfg(feature = "voice")]
    let mut agent_client: Option<crate::agent::LlmClient> = None;
    // Conversation history for the agent (OpenAI chat messages), reset per call.
    #[cfg(feature = "voice")]
    let mut history: Vec<serde_json::Value> = Vec::new();
    // Aokie's last spoken line (greeting or reply) — for the self-echo guard.
    #[cfg(feature = "voice")]
    let mut last_bot_reply = String::new();
    // Half-duplex gate: while Aokie is speaking (+ a short tail) inbound audio is
    // discarded so we never transcribe our own TTS echoing back over the line.
    #[cfg(feature = "voice")]
    let mut mute_stt_until: Option<std::time::Instant> = None;
    // Monotonic transcript turn index (caller + bot share one sequence), reset
    // per call. 1-based to match the simulated-call convention (`turn.1.final`,
    // `turn.2.final`, …) so real + simulated calls dedup + display identically.
    #[cfg(feature = "voice")]
    let mut turn_index: u32 = 1;

    // Per-call bookkeeping. `pending_incoming` holds a just-rung call whose
    // `aokie.call.incoming` we delay briefly so the CLIP (caller id) can be
    // folded into its `from` field — matching the mock's `{from, at}` shape.
    // `answered_corr` records the call we've already auto-answered so we
    // answer exactly once.
    //
    // Auto-answer fires IMMEDIATELY when a call appears — NOT after a ring
    // delay — because on some dongles the SCO/audio channel that comes up
    // right after the ring blocks the radio's main loop, so a late answer
    // never gets serviced. Answering in the brief pre-SCO window is what
    // gets the AT+ATA out. (Emitting `aokie.call.incoming` still waits for
    // the caller id; only the answer is hurried.)
    let mut caller_id: Option<String> = None;
    let mut current_corr: Option<String> = None;
    let mut pending_incoming: Option<(String, Instant)> = None;
    let mut answered_corr: Option<String> = None;
    // Stage-2 diagnostic: which call we've played the outbound-audio test chime
    // to (verifies the SCO-OUT path reaches the caller on this dongle).
    let mut toned_corr: Option<String> = None;

    loop {
        let mut idle = true;

        while let Some(ev) = bt.try_recv_event() {
            idle = false;
            handle_event(
                ev,
                &mut caller_id,
                &mut current_corr,
                &mut pending_incoming,
                outbox,
                sink,
                &status,
            );
        }

        // Flush a buffered incoming call once the caller id is known or the
        // grace window elapses.
        if let Some((corr, since)) = pending_incoming.clone() {
            if caller_id.is_some() || since.elapsed() > Duration::from_millis(800) {
                let from = caller_id.clone().unwrap_or_else(|| "unknown".to_string());
                emit(
                    outbox,
                    sink,
                    aokie_core::events::aokie_event(
                        "aokie.call.incoming",
                        &corr,
                        json!({"from": from, "at": aokie_core::events::now_iso8601()}),
                    ),
                );
                pending_incoming = None;
            }
        }

        // Auto-answer ASAP: the instant a call is present and not yet answered,
        // send the answer — before the audio channel comes up and freezes the
        // loop. Answer exactly once per call (tracked by `answered_corr`).
        if auto_answer {
            match current_corr.as_deref() {
                Some(corr)
                    if answered_corr.as_deref() != Some(corr)
                        && !status.call_active.load(Ordering::Relaxed) =>
                {
                    match bt.answer_call() {
                        Ok(()) => eprintln!("[aokie-plugin] auto-answered incoming call (immediate)"),
                        Err(e) => eprintln!("[aokie-plugin] auto-answer failed: {e}"),
                    }
                    answered_corr = Some(corr.to_string());
                    idle = false;
                }
                None => answered_corr = None, // call cleared — ready for the next
                _ => {}
            }
        }

        // Stage-2 diagnostic: once the call's audio channel is up (sample rate
        // becomes non-zero), play a short two-note chime to the caller to verify
        // the OUTBOUND SCO path actually reaches the phone on this dongle. Real
        // TTS speech replaces this once outbound audio is confirmed. Gated by
        // settings.answerTone.
        if answer_tone {
            match current_corr.as_deref() {
                Some(corr) if toned_corr.as_deref() != Some(corr) => {
                    let sr = bt.get_sample_rate();
                    if sr > 0 {
                        let tone = greeting_tone(sr);
                        eprintln!(
                            "[aokie-plugin] answerTone: sending {} samples @ {}Hz to the caller",
                            tone.len(),
                            sr
                        );
                        bt.send_audio(&tone);
                        toned_corr = Some(corr.to_string());
                        idle = false;
                    }
                }
                None => toned_corr = None,
                _ => {}
            }
        }

        // Greet the caller with real TTS speech once the SCO audio channel is up
        // (voice build). Plays exactly once per call. Without the voice feature
        // this is a no-op (greeted_corr just tracks the call).
        match current_corr.as_deref() {
            Some(corr) if greeted_corr.as_deref() != Some(corr) => {
                let sr = bt.get_sample_rate();
                if sr > 0 {
                    #[cfg(feature = "voice")]
                    if let Some(text) = greeting.as_deref() {
                        let dur = tts_speak(bt, &mut tts, text, sr);
                        mute_stt_until = Some(Instant::now() + dur + Duration::from_millis(400));
                        stt_buf.clear();
                        stt_had_speech = false;
                        stt_silence = Duration::ZERO;
                        emit_turn(outbox, sink, corr, turn_index, "bot", text);
                        turn_index += 1;
                        history.push(serde_json::json!({ "role": "assistant", "content": text }));
                        last_bot_reply = text.to_string();
                    }
                    greeted_corr = Some(corr.to_string());
                    idle = false;
                }
            }
            None => {
                greeted_corr = None;
                #[cfg(feature = "voice")]
                {
                    turn_index = 1;
                    history.clear();
                    last_bot_reply.clear();
                }
            }
            _ => {}
        }

        // Inbound caller audio.
        #[cfg(not(feature = "voice"))]
        while bt.try_recv_audio().is_some() {
            idle = false;
        }
        // Voice build: energy-VAD segment the caller's speech → ship each finished
        // utterance to the STT worker. ~350 RMS (i16 units) gates speech; ~700 ms
        // of trailing silence ends an utterance; sub-350 ms blips are dropped.
        #[cfg(feature = "voice")]
        {
            const SPEECH_RMS: f32 = 350.0;
            let endpoint = stt_endpoint;
            let muted = mute_stt_until.is_some_and(|t| Instant::now() < t);
            while let Some(frame) = bt.try_recv_audio() {
                idle = false;
                if muted || current_corr.is_none() {
                    continue;
                }
                let rms = crate::voice::frame_rms(&frame.samples);
                let f16 = crate::voice::to_f32_16k(&frame.samples, frame.sample_rate as u32);
                let frame_dur = Duration::from_secs_f32(
                    frame.samples.len() as f32 / frame.sample_rate.max(1) as f32,
                );
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
                    let _ = stt_tx.send(std::mem::take(&mut stt_buf));
                } else {
                    stt_buf.clear();
                }
                stt_had_speech = false;
                stt_silence = Duration::ZERO;
            }
            // Finished transcripts → aokie.call.turn.final (the flow's conversation hook,
            // and the recording source). If the in-plugin agent is on, also answer
            // the caller directly here — streaming the LLM + speaking each sentence.
            while let Ok(text) = stt_result_rx.try_recv() {
                idle = false;
                if let Some(corr) = current_corr.clone() {
                    // Drop a transcript that's really Aokie's own reply echoing back
                    // (belt-and-suspenders over the half-duplex mute) so it never
                    // records it as a caller turn or answers itself.
                    if agent_enabled && looks_like_echo(&text, &last_bot_reply) {
                        eprintln!("[aokie-plugin] ignored self-echo: {text:?}");
                        continue;
                    }
                    eprintln!("[aokie-plugin] heard [turn {turn_index}]: {text:?}");
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
                                    let c = crate::agent::LlmClient::new(ep, None);
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
                            let mut messages =
                                vec![serde_json::json!({ "role": "system", "content": agent_persona })];
                            messages.extend(history.iter().cloned());
                            // Mute STT for the WHOLE reply as it streams. Sentences
                            // synthesize faster than they play, so the audio keeps
                            // playing (queued) after synthesis finishes; muting only
                            // the last sentence let the tail echo back and Aokie
                            // answered itself. Track cumulative playback from t0.
                            let t0 = Instant::now();
                            let mut reply_dur = Duration::ZERO;
                            eprintln!("[aokie-plugin] agent replying (streaming)…");
                            let outcome = client.stream_reply(serde_json::json!(messages), |sentence| {
                                eprintln!("[aokie-plugin] agent sentence (+{:?}): {sentence:?}", t0.elapsed());
                                reply_dur += tts_speak(bt, &mut tts, sentence, sr);
                                let plays_until = (t0 + reply_dur).max(Instant::now());
                                mute_stt_until = Some(plays_until + Duration::from_millis(600));
                                true
                            });
                            // Cover audio still queued after the last chunk synthesized.
                            let plays_until = (t0 + reply_dur).max(Instant::now());
                            mute_stt_until = Some(plays_until + Duration::from_millis(800));
                            match outcome {
                                Ok(full) if !full.trim().is_empty() => {
                                    let full = full.trim().to_string();
                                    history.push(serde_json::json!({ "role": "assistant", "content": full }));
                                    emit_turn(outbox, sink, &corr, turn_index, "bot", &full);
                                    turn_index += 1;
                                    last_bot_reply = full;
                                    // Discard anything captured while we replied.
                                    stt_buf.clear();
                                    stt_had_speech = false;
                                    stt_silence = Duration::ZERO;
                                }
                                Ok(_) => {}
                                Err(e) => eprintln!("[aokie-plugin] agent reply failed: {e}"),
                            }
                        }
                    }
                }
            }
        }

        loop {
            match control_rx.try_recv() {
                Ok(RadioControl::Answer) => {
                    if let Err(e) = bt.answer_call() {
                        eprintln!("[aokie-plugin] radio answer failed: {e}");
                    }
                }
                Ok(RadioControl::Reject) => {
                    if let Err(e) = bt.reject_call() {
                        eprintln!("[aokie-plugin] radio reject failed: {e}");
                    }
                }
                Ok(RadioControl::Hangup) => {
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
                                "aokie.hardware.error",
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
                        // stale enabled-copy in the desktop's runtime cache) —
                        // otherwise the caller is answered twice.
                        eprintln!("[aokie-plugin] ignoring operatorSpeak (agent owns replies): {text:?}");
                    } else {
                        let dur = tts_speak(bt, &mut tts, &text, bt.get_sample_rate());
                        mute_stt_until = Some(Instant::now() + dur + Duration::from_millis(400));
                        stt_buf.clear();
                        stt_had_speech = false;
                        stt_silence = Duration::ZERO;
                        // Record Aokie's spoken reply as a bot transcript turn.
                        if let Some(corr) = current_corr.clone() {
                            emit_turn(outbox, sink, &corr, turn_index, "bot", &text);
                            turn_index += 1;
                        }
                    }
                    #[cfg(not(feature = "voice"))]
                    eprintln!(
                        "[aokie-plugin] operatorSpeak ({} chars) — voice feature not built",
                        text.chars().count()
                    );
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

/// Map one `BluetoothEvent` to the `aokie.*` contract + update shared status.
/// Available on all targets (it only touches `BluetoothEvent`, which is not
/// Windows-gated) so it stays unit-testable.
fn handle_event(
    ev: aokie_dongle::bluetooth::BluetoothEvent,
    caller_id: &mut Option<String>,
    current_corr: &mut Option<String>,
    pending_incoming: &mut Option<(String, std::time::Instant)>,
    outbox: Option<&Outbox>,
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
                aokie_event("aokie.dongle.ready", "radio", json!({"address": addr, "source": "radio"})),
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
                aokie_event("aokie.phone.connected", "radio", json!({"address": addr})),
            );
        }
        E::DeviceDisconnected(addr) => {
            status.connected.store(false, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event("aokie.phone.disconnected", "radio", json!({"address": addr})),
            );
        }
        E::CallIncoming => {
            // Some phones emit the ring / callsetup indicator more than once for
            // a single call. Only start a NEW call (fresh corr) when we aren't
            // already handling one, so the incoming event + greeting fire exactly
            // once per call (fixes the double greeting).
            if current_corr.is_none() {
                let corr = format!("call_{}", uuid::Uuid::new_v4().simple());
                *current_corr = Some(corr.clone());
                *caller_id = None;
                *status.current_caller.lock().unwrap() = None;
                *pending_incoming = Some((corr, std::time::Instant::now()));
            }
        }
        E::CallerId(num) => {
            *caller_id = Some(num.clone());
            *status.current_caller.lock().unwrap() = Some(num);
        }
        E::CallRinging => {
            if let Some(corr) = current_corr.as_ref() {
                emit(
                    outbox,
                    sink,
                    aokie_event("aokie.call.ringing", corr, json!({"at": now_iso8601()})),
                );
            }
        }
        E::CallAnswered => {
            status.call_active.store(true, Ordering::Relaxed);
            if let Some(corr) = current_corr.as_ref() {
                emit(
                    outbox,
                    sink,
                    aokie_event("aokie.call.answered", corr, json!({"at": now_iso8601()})),
                );
            }
        }
        E::CallTerminated => {
            status.call_active.store(false, Ordering::Relaxed);
            if let Some(corr) = current_corr.take() {
                emit(
                    outbox,
                    sink,
                    aokie_event(
                        "aokie.call.ended",
                        &corr,
                        json!({"at": now_iso8601(), "reason": "remote_or_operator"}),
                    ),
                );
            }
            *caller_id = None;
            *pending_incoming = None;
            *status.current_caller.lock().unwrap() = None;
        }
        E::AudioConnected { codec, sample_rate } => {
            let corr = current_corr.clone().unwrap_or_else(|| "radio".to_string());
            emit(
                outbox,
                sink,
                aokie_event(
                    "aokie.call.audio.connected",
                    &corr,
                    json!({"codec": codec, "sampleRate": sample_rate}),
                ),
            );
        }
        E::AudioDisconnected => {
            let corr = current_corr.clone().unwrap_or_else(|| "radio".to_string());
            emit(
                outbox,
                sink,
                aokie_event("aokie.call.audio.disconnected", &corr, json!({})),
            );
        }
        E::SmsReceived(p) => {
            let corr = format!("sms_{}", uuid::Uuid::new_v4().simple());
            emit(
                outbox,
                sink,
                aokie_event(
                    "aokie.sms.received",
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
                    "aokie.sms.sent",
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
                aokie_event("aokie.hardware.error", "radio", json!({"message": e})),
            );
        }
    }
}

// ── Non-Windows: no radio ────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
pub fn spawn(
    _data_dir: std::path::PathBuf,
    _preferred_path: Option<String>,
    _auto_answer: bool,
    _answer_tone: bool,
    _reenumerate_hwid: Option<String>,
    _greeting: Option<String>,
) -> Result<RadioHandle, String> {
    Err("the Aokie radio is only supported on Windows (WinUSB)".to_string())
}
