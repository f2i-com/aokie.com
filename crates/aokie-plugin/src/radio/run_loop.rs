//! The radio thread main loop: call lifecycle, STT/LLM/TTS orchestration.

#[allow(unused_imports)]
use super::*;

/// The radio poll loop: drain events â†’ map+emit; buffer the incoming-call
/// emission until the caller id lands (or a short timeout); drain audio
/// (Stage 2 feeds the AI here); service control requests. Runs until the
/// control channel closes or a Shutdown is received.
#[cfg(target_os = "windows")]
// `greeting` is only mutated (via RadioControl::Configure) in the voice build.
#[cfg_attr(not(feature = "voice"), allow(unused_mut))]
pub(super) fn run_loop(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: std::sync::mpsc::Receiver<RadioControl>,
    status: Arc<RadioStatus>,
    auto_answer: bool,
    answer_tone: bool,
    mut greeting: Option<String>,
    host_rpc: Arc<crate::host_rpc::HostRpc>,
    data_dir: &std::path::Path,
    remote_media: crate::remote_media::RemoteMediaHandle,
) {
    #[cfg(not(all(target_os = "windows", feature = "voice")))]
    let _ = &host_rpc;
    use std::time::Duration;
    #[cfg(feature = "voice")]
    use std::time::Instant;

    #[cfg(feature = "voice")]
    let realtime_selected =
        std::env::var("AOKIE_REALTIME_VOICE_MODE").as_deref() == Ok("desktop_realtime");
    #[cfg(feature = "voice")]
    let realtime_config = match realtime_runtime_config() {
        Ok(config) => {
            status
                .realtime_destination
                .lock()
                .unwrap()
                .clone_from(&config.as_ref().map(|config| config.destination.clone()));
            *status.realtime_error.lock().unwrap() = None;
            config
        }
        Err(error) => {
            eprintln!("[aokie-plugin] Desktop realtime voice unavailable: {error}");
            *status.realtime_error.lock().unwrap() = Some(error);
            None
        }
    };
    #[cfg(feature = "voice")]
    {
        status
            .realtime_selected
            .store(realtime_selected, Ordering::Relaxed);
        status.realtime_ready.store(false, Ordering::Relaxed);
        if !realtime_selected {
            *status.realtime_destination.lock().unwrap() = None;
            *status.realtime_error.lock().unwrap() = None;
        }
    }

    // Phase 2: synthesis runs on the synth WORKER (in-process engine loaded
    // lazily there on the first thing Aokie says; the HTTP TTS endpoint +
    // its sticky per-call fallback live there too). The radio thread paces
    // the worker's PCM into SCO ~200 ms ahead of playout — see tts_speak.
    #[cfg(feature = "voice")]
    let synth = crate::synth::SynthHandle::spawn();
    #[cfg(feature = "voice")]
    let private_consult = crate::private_consult::PrivateConsultHandle::spawn(remote_media.clone())
        .map_err(|error| {
            eprintln!("[aokie-plugin] private consult worker unavailable: {error}");
            error
        })
        .ok();
    #[cfg(feature = "voice")]
    let mut consult_started_for: Option<String> = None;
    let _ = &greeting; // used only in the voice build / greeting block below

    // AOK-VOICE-001: fast asset preflight (presence-only — radio start stays
    // instant) seeds the shared voice-status slots BEFORE any call can arrive:
    // a deleted model / missing ONNX Runtime DLL (with no HTTP endpoint
    // substituting) is a KNOWN failure that degrades plugin.health and blocks
    // auto-answer below, instead of being discovered mid-call by answering a
    // caller into silence. Corruption is caught by the live engine loads,
    // which update the same slots.
    #[cfg(feature = "voice")]
    {
        if realtime_selected {
            // Realtime owns normal-call STT and synthesis upstream. Do not
            // download, self-test, or eagerly load the local speech bundles at
            // startup; they remain lazy for explicit legacy lanes.
            // The presence-only probe is intentionally retained: a caller
            // classified into the manager/screened legacy lane must still
            // ring through if its local fallback cannot hear or speak.
            let pf = crate::voice::preflight_assets();
            *status.stt_error.lock().unwrap() = pf.stt_error;
            *status.tts_error.lock().unwrap() = pf.tts_error;
            *status.self_test.lock().unwrap() = Some(VoiceSelfTest {
                ok: true,
                at: aokie_core::events::now_iso8601(),
                duration_ms: 0,
                detail: "skipped: Desktop realtime selected; local speech remains lazy for legacy policy calls"
                    .to_string(),
            });
            eprintln!(
                "[aokie-plugin] local speech downloads/self-test skipped for Desktop realtime voice (presence-only legacy preflight retained)"
            );
        } else {
            // DIST-001: a clean machine obtains immutable, digest-pinned model
            // bundles automatically. Downloads happen on this background radio
            // thread, resume from `.part` files, and complete before auto-answer
            // can arm. A remote speech endpoint suppresses its local bundle.
            let distribution = crate::model_distribution::ensure_required_models();
            let mut pf = crate::voice::preflight_assets();
            if distribution.stt_error.is_some() {
                pf.stt_error = distribution.stt_error;
            }
            if distribution.tts_error.is_some() {
                pf.tts_error = distribution.tts_error;
            }
            if let Some(e) = &pf.stt_error {
                eprintln!("[aokie-plugin] voice preflight: {e}");
            }
            if let Some(e) = &pf.tts_error {
                eprintln!("[aokie-plugin] voice preflight: {e}");
            }
            if pf.stt_error.is_none() && pf.tts_error.is_none() {
                eprintln!(
                    "[aokie-plugin] voice preflight OK (ORT + STT/TTS assets or endpoints present)"
                );
            }
            let preflight_failed = pf.stt_error.is_some() || pf.tts_error.is_some();
            *status.stt_error.lock().unwrap() = pf.stt_error;
            *status.tts_error.lock().unwrap() = pf.tts_error;

            // VOICE-001: the measured loopback self-test — EXERCISE the engines
            // (TTS→STT round trip of a known phrase) before auto-answer may arm,
            // so a corrupt model / broken provider / silent synthesis is caught at
            // startup, not by the first caller. Runs on its own thread (the ONNX
            // loads are heavy); auto-answer stays blocked until a report lands.
            // Skip cases write an OK report with the reason so arming isn't held
            // hostage: HTTP endpoints replace the local engines this test covers,
            // and a failed preflight already blocks via its own slots.
            let skip_reason: Option<String> = if std::env::var("AOKIE_SKIP_SELF_TEST").as_deref()
                == Ok("1")
            {
                Some("skipped (AOKIE_SKIP_SELF_TEST=1)".to_string())
            } else if std::env::var("AOKIE_STT_ENDPOINT").is_ok_and(|v| !v.trim().is_empty())
                || std::env::var("AOKIE_TTS_ENDPOINT").is_ok_and(|v| !v.trim().is_empty())
            {
                Some(
                    "skipped: HTTP speech endpoint(s) configured - the local-engine loopback does not cover them"
                        .to_string(),
                )
            } else if preflight_failed {
                Some("skipped: asset preflight already failed (see stt/tts errors)".to_string())
            } else {
                None
            };
            match skip_reason {
                Some(reason) => {
                    eprintln!("[aokie-plugin] voice self-test {reason}");
                    *status.self_test.lock().unwrap() = Some(VoiceSelfTest {
                        ok: true,
                        at: aokie_core::events::now_iso8601(),
                        duration_ms: 0,
                        detail: reason,
                    });
                }
                None => {
                    let status_st = status.clone();
                    let spawned = std::thread::Builder::new()
                        .name("aokie-voice-selftest".to_string())
                        .spawn(move || {
                            let started = std::time::Instant::now();
                            // A panic inside the ONNX stack must still produce a
                            // report — an empty slot blocks auto-answer forever.
                            let outcome =
                                std::panic::catch_unwind(crate::voice::run_loopback_self_test)
                                    .unwrap_or_else(|_| {
                                        Err("self-test panicked inside the speech stack"
                                            .to_string())
                                    });
                            let report = match outcome {
                                Ok(heard) => VoiceSelfTest {
                                    ok: true,
                                    at: aokie_core::events::now_iso8601(),
                                    duration_ms: started.elapsed().as_millis() as u64,
                                    detail: format!("loopback ok - heard {heard:?}"),
                                },
                                Err(e) => VoiceSelfTest {
                                    ok: false,
                                    at: aokie_core::events::now_iso8601(),
                                    duration_ms: started.elapsed().as_millis() as u64,
                                    detail: e,
                                },
                            };
                            eprintln!(
                                "[aokie-plugin] voice self-test {} in {}ms — {}",
                                if report.ok {
                                    "PASSED"
                                } else {
                                    "FAILED (auto-answer blocked)"
                                },
                                report.duration_ms,
                                report.detail
                            );
                            *status_st.self_test.lock().unwrap() = Some(report);
                        });
                    if let Err(e) = spawned {
                        // Can't run it — never leave the slot empty (permanent block).
                        eprintln!("[aokie-plugin] voice self-test thread failed to start: {e}");
                        *status.self_test.lock().unwrap() = Some(VoiceSelfTest {
                            ok: false,
                            at: aokie_core::events::now_iso8601(),
                            duration_ms: 0,
                            detail: format!("self-test thread failed to start: {e}"),
                        });
                    }
                }
            }
        }
    }
    // Non-voice builds never run the loopback: record the skip so any reader
    // (health) sees a settled state instead of "still running" forever.
    #[cfg(not(feature = "voice"))]
    {
        *status.self_test.lock().unwrap() = Some(VoiceSelfTest {
            ok: true,
            at: aokie_core::events::now_iso8601(),
            duration_ms: 0,
            detail: "skipped: no voice output compiled".to_string(),
        });
    }
    // Warn-once bookkeeping for the auto-answer voice block (per call id).
    let mut voice_block_logged_call: Option<String> = None;

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
    let (stt_tx, stt_result_rx, probe_result_rx) = {
        let (utter_tx, utter_rx) = std::sync::mpsc::channel::<SttWork>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<SttResult>();
        // Dedicated probe-result channel: control probes must never be
        // confused with (or consume) final-utterance results.
        let (probe_tx, probe_rx) = std::sync::mpsc::channel::<SttResult>();
        let initial_stt_endpoint = std::env::var("AOKIE_STT_ENDPOINT").ok();
        let worker_gen = stt_current_gen.clone();
        let worker_status = status.clone();
        std::thread::Builder::new()
            .name("aokie-stt".into())
            .spawn(move || {
                // CONSENT-001: the operator denied the `transcription` scope —
                // NO caller audio may reach any STT engine (in-process or HTTP).
                // Set by the connector before radio start from the consent gate;
                // frames are dropped here at the last hop before an engine.
                let stt_disabled = std::env::var("AOKIE_STT_DISABLED")
                    .map(|v| v == "1")
                    .unwrap_or(false);
                if stt_disabled {
                    eprintln!(
                        "[aokie-plugin] transcription consent DENIED — STT worker will drop all audio"
                    );
                }
                let mut engine: Option<crate::voice::SttEngine> = None;
                let mut http_stt = HttpSpeechFallback::new(initial_stt_endpoint);
                while let Ok(work) = utter_rx.recv() {
                    // Control probes first: local engine only (no HTTP egress),
                    // answered on the probe channel, generation-gated like
                    // everything else. Best-effort — a failed probe is silence,
                    // never an error path.
                    if let SttWork::Probe { generation, lane, samples } = &work {
                        if stt_disabled || *generation != worker_gen.load(Ordering::Relaxed) {
                            continue;
                        }
                        if engine.is_none() {
                            match crate::voice::SttEngine::load() {
                                Ok(e) => {
                                    eprintln!("[aokie-plugin] STT engine loaded");
                                    *worker_status.stt_error.lock().unwrap() = None;
                                    engine = Some(e);
                                }
                                Err(e) => {
                                    eprintln!("[aokie-plugin] STT load failed (probe): {e}");
                                    *worker_status.stt_error.lock().unwrap() =
                                        Some(format!("the STT engine failed to load: {e}"));
                                    continue;
                                }
                            }
                        }
                        if let Some(eng) = engine.as_mut() {
                            let _busy = SttBusyGuard::set(&worker_status, samples.len());
                            match eng.transcribe(samples) {
                                Ok(text) if !text.is_empty() => {
                                    let _ = probe_tx.send(SttResult {
                                        generation: *generation,
                                        // Echo the probe's LANE id so each
                                        // consumer can drop foreign results
                                        // (stale-probe fencing, 066d2237).
                                        utterance: *lane,
                                        text,
                                    });
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    eprintln!("[aokie-plugin] STT probe transcribe failed: {e}")
                                }
                            }
                        }
                        continue;
                    }
                    let (generation, utterance, buf) = match work {
                        SttWork::Utterance { .. } if stt_disabled => continue,
                        SttWork::Utterance {
                            generation,
                            utterance,
                            samples,
                        } => (generation, utterance, samples),
                        SttWork::Probe { .. } => continue, // handled above
                        SttWork::Warm => {
                            if !stt_disabled && engine.is_none() {
                                match crate::voice::SttEngine::load() {
                                    Ok(e) => {
                                        eprintln!("[aokie-plugin] STT engine pre-warmed");
                                        *worker_status.stt_error.lock().unwrap() = None;
                                        engine = Some(e);
                                    }
                                    Err(e) => {
                                        eprintln!("[aokie-plugin] STT pre-warm failed: {e}");
                                        *worker_status.stt_error.lock().unwrap() =
                                            Some(format!("the STT engine failed to load: {e}"));
                                    }
                                }
                            }
                            continue;
                        }
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
                    // Busy for the rest of this job (HTTP or local): the loop
                    // watchdog reports a wedged transcription with its size.
                    let _busy = SttBusyGuard::set(&worker_status, buf.len());
                    if let Some(endpoint) = http_stt.endpoint_for_call().map(str::to_string) {
                        // Hardened per-endpoint client (AOK-ENDPOINT-001); a rejected
                        // endpoint takes the same sticky fallback path as a failed request.
                        match http_speech_client(&endpoint)
                            .and_then(|client| http_stt_transcribe(&client, &endpoint, &buf))
                        {
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
                                // AOK-VOICE-001: a working load clears any
                                // preflight/previous failure for this half.
                                *worker_status.stt_error.lock().unwrap() = None;
                                engine = Some(e);
                            }
                            Err(e) => {
                                eprintln!("[aokie-plugin] STT load failed: {e}");
                                // AOK-VOICE-001: a KNOWN hearing failure —
                                // degrade health + block auto-answer.
                                *worker_status.stt_error.lock().unwrap() = Some(format!(
                                    "the STT engine failed to load: {e}"
                                ));
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
        (utter_tx, res_rx, probe_rx)
    };
    // VAD / utterance accumulator (all in 16 kHz mono f32, the STT engine's rate).
    #[cfg(feature = "voice")]
    let mut stt_buf: Vec<f32> = Vec::new();
    // Utterances sent to the STT worker whose results haven't come back yet
    // (audit AOK-LIF-002): the termination drain waits for these — bounded —
    // so the caller's last words land BEFORE call.ended.
    #[cfg(feature = "voice")]
    let mut stt_outstanding: usize = 0;
    // Early-start STT (round 3): the utterance is transcribed SPECULATIVELY
    // after a SHORT silence so its text is ready AT the endpoint instead of
    // an STT-latency after it. Resumed speech invalidates the speculation
    // (the grown utterance re-transcribes whole; the stale result is dropped
    // by id, never merged).
    #[cfg(feature = "voice")]
    let mut spec_utterance: Option<u32> = None;
    #[cfg(feature = "voice")]
    let mut stale_specs: Vec<u32> = Vec::new();
    #[cfg(feature = "voice")]
    let mut stt_had_speech = false;
    #[cfg(feature = "voice")]
    let mut stt_silence = std::time::Duration::ZERO;
    // End-of-utterance silence: how long the caller must pause before we treat
    // their turn as finished and transcribe. Lower = snappier replies but risks
    // cutting off mid-sentence pauses. Tunable via AOKIE_STT_ENDPOINT_MS (set from
    // the `sttEndpointMs` connector setting); default 450 ms.
    // §3.11 bounds reconciliation: honour exactly the range `settings.set`
    // accepts (shared consts) — an accepted value must never silently fall
    // back to the default.
    #[cfg(feature = "voice")]
    let stt_endpoint = std::time::Duration::from_millis(
        std::env::var("AOKIE_STT_ENDPOINT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&m| {
                (crate::connector::STT_ENDPOINT_MS_MIN..=crate::connector::STT_ENDPOINT_MS_MAX)
                    .contains(&m)
            })
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
    // AOK-CTRL-001: publish the RUNNING radio's responder ownership so the
    // connector can refuse operatorSpeak truthfully (the radio would drop it).
    #[cfg(feature = "voice")]
    status.agent_enabled.store(agent_enabled, Ordering::Relaxed);
    // Shared with the LLM readiness probe thread (PROC-001): Configure updates
    // land here so the probe always checks the CURRENT endpoint setting.
    #[cfg(feature = "voice")]
    let agent_endpoint: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(
        std::env::var("AOKIE_AI_ENDPOINT")
            .ok()
            .filter(|s| !s.trim().is_empty()),
    ));
    // PROC-001: background LLM readiness probe. Runs ONLY while the in-plugin
    // agent owns replies: re-resolves the endpoint every ~30s and records the
    // outcome in status.llm_error, so a dead/unloaded LLM shows up in
    // plugin.health (and blocks auto-answer below) BEFORE a caller finds out.
    // The keep-alive sender lives in this scope — when the radio loop returns,
    // it drops, the probe's recv_timeout disconnects, and the thread exits.
    #[cfg(feature = "voice")]
    let _llm_probe_stop_tx: Option<std::sync::mpsc::Sender<()>> = if agent_enabled {
        *status.llm_error.lock().unwrap() = Some("LLM readiness probe is pending".to_string());
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let status_probe = status.clone();
        let endpoint_probe = agent_endpoint.clone();
        let spawned = std::thread::Builder::new()
            .name("aokie-llm-probe".to_string())
            .spawn(move || {
                loop {
                    let configured = endpoint_probe.lock().unwrap().clone();
                    let reachable = crate::agent::discover_endpoint(configured.as_deref());
                    let new_error = match reachable {
                        Some(_) => None,
                        None => Some(match &configured {
                            Some(ep) => format!("LLM endpoint {ep} is not answering"),
                            None => "no reachable LLM (tried llama.cpp :8080 and ollama :11434)"
                                .to_string(),
                        }),
                    };
                    {
                        let mut slot = status_probe.llm_error.lock().unwrap();
                        if *slot != new_error {
                            match &new_error {
                                Some(e) => eprintln!(
                                    "[aokie-plugin] LLM readiness: DOWN — {e}; auto-answer is blocked until it recovers"
                                ),
                                None => eprintln!("[aokie-plugin] LLM readiness: ok"),
                            }
                            *slot = new_error;
                        }
                    }
                    match stop_rx.recv_timeout(Duration::from_secs(30)) {
                        // A message or a dropped sender both mean the radio is done.
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            });
        match spawned {
            Ok(_) => Some(stop_tx),
            Err(e) => {
                eprintln!("[aokie-plugin] LLM readiness probe thread failed to start: {e}");
                None
            }
        }
    } else {
        None
    };
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
    // §9.3: when the greeting first became READY but was held for the
    // personalization overlay (bounds the hold; reset per call).
    #[cfg(feature = "voice")]
    let mut greet_hold_started: Option<Instant> = None;
    // Phase 2 dial watchdog: the call id whose stuck outbound attempt we
    // already cancelled — the CHUP is sent exactly once per attempt (ids are
    // never reused, so no reset is needed).
    let mut dial_cancel_sent: Option<String> = None;
    // Dead-air watchdog: one continuously timed SCO outage. A Companion-owned
    // caller must return to Aokie before this path may ever send CHUP.
    let mut no_sco_watchdog = NoScoWatchdog::default();
    // A screened caller may be claimed during the bounded screen message.
    // The human route wins immediately; if it later returns the same caller,
    // re-apply the already-decided screen hangup before Aokie can converse.
    #[cfg(feature = "voice")]
    let mut screened_hangup_pending_for: Option<String> = None;
    // Consecutive CallIncoming events observed while the tracker held an
    // ACTIVE inbound session — the phantom-answer self-heal counter (see the
    // guard in the event loop).
    let mut phantom_ring_count: u32 = 0;
    // Ring-time personalization window: when auto-answer first saw the
    // ringing call (bounds the hold; reset per call).
    #[cfg(feature = "voice")]
    let mut answer_hold_started: Option<Instant> = None;
    // Guide phase 2/5 — LIVE HYPOTHESIS LANE: while the caller speaks (bot
    // idle), the in-progress utterance is re-transcribed every ~600 ms on the
    // probe channel, giving streaming partial text; a STABLE partial starts a
    // SPECULATIVE reply generation so the answer is largely ready at the
    // endpoint instead of starting there.
    #[cfg(all(target_os = "windows", feature = "voice"))]
    let mut live_hyp: Option<String> = None;
    #[cfg(all(target_os = "windows", feature = "voice"))]
    let mut live_hyp_prev: Option<String> = None;
    #[cfg(all(target_os = "windows", feature = "voice"))]
    let mut live_hyp_shipped: usize = 0;
    #[cfg(all(target_os = "windows", feature = "voice"))]
    let mut live_hyp_at: Option<Instant> = None;
    #[cfg(all(target_os = "windows", feature = "voice"))]
    let mut live_probe_in_flight = false;
    #[cfg(all(target_os = "windows", feature = "voice"))]
    let mut spec_reply: Option<ReplyStream> = None;
    // Ring-warm handoff: the off-thread LLM warm CONNECTS a client and parks
    // it here; the loop adopts it so the FIRST caller turn can speculate and
    // reply without the lazy connect (which previously only happened at the
    // first reply — first-turn speculation never fired).
    #[cfg(all(target_os = "windows", feature = "voice"))]
    let pending_agent_client: Arc<Mutex<Option<crate::agent::LlmClient>>> =
        Arc::new(Mutex::new(None));
    // Controls that arrived DURING an agent reply (audit AK-003): the
    // mid-reply poll acts on Hangup/Reject instantly and parks everything
    // else here; the main control loop drains this before its channel.
    let mut pending_controls: std::collections::VecDeque<RadioControl> = Default::default();
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
    // sendAudio: attach the caller turn's PCM (base64 WAV) to the LLM request
    // alongside the transcript — for audio-capable models (Gemma 3n /
    // Qwen2-Audio class). The snapshot is taken where the utterance is sent
    // to STT; capped at ~30 s so a rambling turn can't balloon the request.
    //
    // audioTranscript: after each caller turn a small DETACHED request asks
    // the audio model to correct the STT from the turn's audio; results ride
    // this channel back (tagged with their call id + turn, so a correction
    // for an ENDED call still lands on its transcript row). INDEPENDENT of
    // sendAudio (2026-07-17): "corrections only" (text-only reply model) and
    // "direct audio only" (no side run) both work — the shared per-turn
    // AUDIO CAPTURE machinery runs when either is on (`audio_capture`).
    #[cfg(feature = "voice")]
    let (mut send_audio, mut audio_transcript, audio_capture) = audio_lane_gates(
        agent_enabled,
        std::env::var_os("AOKIE_SEND_AUDIO").is_some(),
        std::env::var_os("AOKIE_AUDIO_TRANSCRIPT").is_some(),
    );
    #[cfg(feature = "voice")]
    let (heard_tx, heard_rx) = std::sync::mpsc::channel::<TranscriptCorrectionResult>();
    // Correction-lane client override (2026-07-17 latency round): an optional
    // separate endpoint for the transcript-correction requests. Built LAZILY
    // inside the first correction's detached thread — LlmClient::new does
    // model discovery over HTTP, which must never run on the radio loop.
    #[cfg(feature = "voice")]
    let transcript_client_cache: std::sync::Arc<
        std::sync::OnceLock<Option<crate::agent::LlmClient>>,
    > = std::sync::Arc::new(std::sync::OnceLock::new());
    // Call screening policy (spec Phase 0): parsed once per radio start.
    #[cfg(feature = "voice")]
    let mut screen_policy = crate::screen::ScreenPolicy::from_env();
    #[cfg(feature = "voice")]
    if screen_policy.is_active() {
        eprintln!("[aokie-plugin] call screening ACTIVE (block list / accept pattern / private-number policy)");
    }
    // Audio capture (sendAudio OR audioTranscript): utterance-id → PCM,
    // written at every STT send and consumed by the result drains into the
    // pending turn (exact pairing — see PendingTurn::audio).
    #[cfg(feature = "voice")]
    let mut utt_audio: std::collections::VecDeque<(u32, Vec<i16>)> = Default::default();
    // When AOKIE_AGENT_HANGUP is set (the `agentHangup` setting) the agent ends
    // the call itself once the caller's request is fully handled: it says a brief
    // goodbye, then hangs up (AT+CHUP) so the caller doesn't have to. The LLM
    // signals completion with an [[END_CALL]] marker, which is stripped before the
    // farewell is spoken/recorded. Only meaningful with the agent on.
    #[cfg(feature = "voice")]
    let agent_hangup = agent_enabled && std::env::var_os("AOKIE_AGENT_HANGUP").is_some();
    // Phase 4: the AUTOMATIC spoken hold juggle — tell the current caller to
    // hold, swap to the newcomer to ask THEM to hold, swap BACK and resume.
    // OFF by default; armed by the `autoHoldQueue` setting (requires
    // `holdAndCallWaiting`). v2: after the z49 live incident (a raw double
    // CHLD=2 tore both calls down on the Pixel — callheld 2→0 — and the old
    // code assumed success), every swap outcome is now VERIFIED against the
    // callheld indicator + an on-demand AT+CLCC before session state moves;
    // see the juggle block + judge_accept/judge_swap_back. With
    // `holdAndCallWaiting` on but this off, call waiting stays in the proven
    // observe/operator modes (aokie.call.waiting + call.switchboard /
    // call.activate).
    #[cfg(feature = "voice")]
    let auto_hold = agent_enabled && std::env::var_os("AOKIE_AUTO_HOLD").is_some();
    // The waiting callId whose auto-hold juggle already ran (so the pump does
    // not re-trigger every loop pass while that caller sits on hold).
    #[cfg(feature = "voice")]
    let mut auto_hold_done_for: Option<String> = None;
    // A held caller was just promoted (their call ended-neighbour retrieved
    // them): the greeting block speaks the "thanks for holding" line to THIS
    // callId instead of the normal greeting, exactly once. Written by the
    // shared reconciliation block; read only in the voice greeting path.
    #[cfg_attr(not(feature = "voice"), allow(unused_assignments, unused_variables))]
    let mut promote_greet_for: Option<String> = None;
    // Phase 4: a caller retrieved/restored from hold MID-CONVERSATION (they
    // were already greeted, so the promoted greeting doesn't apply) hears
    // HOLD_PRIMARY_RESUME_LINE once their audio path is back — see the hook
    // after the per-call reset block. (call id, when set); expires after 10s
    // so it can never leak into a later call.
    #[cfg_attr(not(feature = "voice"), allow(unused_assignments, unused_variables))]
    let mut resume_line_for: Option<(String, std::time::Instant)> = None;
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
    // Operator cap on how long an [[important]] span may resist an overlap.
    #[cfg(feature = "voice")]
    let protected_max_ms = crate::speech_plan::protected_max_ms_from_env();
    // The NEXT caller turn began as OVERLAP capture (spoken while the bot was
    // talking): recorded on the turn so readers don't misread record order as
    // speech order. Set at every overlap-seed site, cleared when a turn flushes.
    #[cfg(feature = "voice")]
    let mut turn_overlapped = false;
    // Back-dated speech-start estimate for that overlap turn (its audio began
    // roughly its own duration before it was seeded).
    #[cfg(feature = "voice")]
    let mut turn_overlap_at: Option<String> = None;

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
    // Submission only proves AT+CHUP was queued. Completion waits for this
    // exact call to disappear from physical truth; a write error, ownership
    // race, or bounded timeout fails back to Aokie.
    let mut pending_companion_end_caller: Option<PendingCompanionEndCaller> = None;
    // Which generation the voice pipeline is configured for; a change (new
    // call OR idle) resets per-call voice state and re-stamps the STT gate.
    #[cfg(feature = "voice")]
    let mut voice_call_gen: u64 = 0;
    #[cfg(feature = "voice")]
    let mut realtime_lane: Option<RealtimeCallLane> = None;
    #[cfg(feature = "voice")]
    let mut realtime_legacy_call: Option<String> = None;
    #[cfg(feature = "voice")]
    let mut realtime_failed_call: Option<(String, String)> = None;
    #[cfg(feature = "voice")]
    let mut realtime_resume_call: Option<String> = None;
    #[cfg(feature = "voice")]
    let mut realtime_answered_at: Option<(String, Instant)> = None;
    #[cfg(feature = "voice")]
    let mut realtime_midcall_failure: Option<(
        String,
        crate::remote_media::AokieOwnerFence,
        String,
    )> = None;
    #[cfg(feature = "voice")]
    let mut realtime_terminal_call: Option<(String, Instant, u8)> = None;
    #[cfg(feature = "voice")]
    let mut realtime_deferred_policy_failure: Option<(String, String)> = None;
    // Phase 4 isolation: THE current caller's conversational state — see
    // [`CallVoiceContext`]. Swapped for a fresh instance in the per-call
    // reset block (reset-by-construction: a new per-caller field cannot
    // be forgotten there); the switchboard PARKS it per caller across
    // hold/resume instead of dropping it.
    let mut ctx = CallVoiceContext::fresh(None);
    // Phase 4 switchboard: the PARKED caller — their session (out of the
    // tracker, no terminal bookkeeping) + their whole conversational
    // context. Max ONE in v1: plain CHLD=2 (this phone's only mode) is
    // ambiguous with more legs. The status mirrors (RadioStatus::
    // parked_call/waiting_call) are the connector's validation view.
    let mut parked: Option<(crate::call_session::CallSession, CallVoiceContext)> = None;
    // A resume in progress: the per-call reset block installs THIS context
    // (the parked caller's, conversation intact) instead of a fresh one.
    let mut pending_ctx_restore: Option<CallVoiceContext> = None;
    // Last observed callheld indicator value — the attribution edge
    // detector for "foreground vanished" (→2) / "parked vanished" (→0)
    // while a caller is parked.
    let mut prev_call_held: u64 = 0;
    // AOK-CTRL-001: call-level max-silence watchdog (agent mode). Created when
    // the greeting arms the conversation, dropped at every call boundary.
    #[cfg(feature = "voice")]
    let silence_window = max_silence_window();

    // Phase-0 observability: a stalled run_loop reports ITSELF — the watchdog
    // compares the iteration beat every 5 s and, on a freeze, logs the phase
    // breadcrumb plus the STT worker's busy state.
    let loop_alive = Arc::new(AtomicBool::new(true));
    {
        let status = status.clone();
        let alive = loop_alive.clone();
        let _ = std::thread::Builder::new()
            .name("aokie-loop-watchdog".into())
            .spawn(move || {
                let mut last = u64::MAX;
                loop {
                    std::thread::sleep(Duration::from_secs(5));
                    if !alive.load(Ordering::Relaxed) {
                        return;
                    }
                    let beat = status.loop_beat.load(Ordering::Relaxed);
                    if beat == last && beat != 0 {
                        let stt = match *status.stt_busy.lock().unwrap() {
                            Some((at, n)) => {
                                format!("BUSY {}ms on {n} samples", at.elapsed().as_millis())
                            }
                            None => "idle".to_string(),
                        };
                        eprintln!(
                            "[aokie-plugin] WATCHDOG: run_loop stalled >5s in phase {} (beat {beat}); stt worker {stt}",
                            status.loop_phase.load(Ordering::Relaxed)
                        );
                    }
                    last = beat;
                }
            });
    }
    struct LoopAlive(Arc<AtomicBool>);
    impl Drop for LoopAlive {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
    let _loop_alive = LoopAlive(loop_alive);

    // autoConnectPhone (2026-07-17): once the dongle is ready, page the last
    // connected phone from OUR side so the receptionist line comes up when the
    // desktop app starts — no manual "Reconnect" click. The outbound page can
    // stall if the phone's stack is mid-teardown (HARD-001), so retry a few
    // times with spacing; then stop and leave manual Reconnect available. The
    // target is the last phone that actually connected (persisted below),
    // falling back to the sole bonded device.
    let auto_connect_enabled = std::env::var("AOKIE_AUTO_CONNECT_PHONE")
        .map(|v| v != "0")
        .unwrap_or(true);
    let last_phone_path = data_dir.join("last-phone.txt");
    let mut auto_connect_next: Option<std::time::Instant> = if auto_connect_enabled {
        Some(std::time::Instant::now() + std::time::Duration::from_secs(4))
    } else {
        None
    };
    let mut auto_connect_attempts: u32 = 0;
    const AUTO_CONNECT_MAX_ATTEMPTS: u32 = 6;
    let mut last_phone_persisted: Option<String> = std::fs::read_to_string(&last_phone_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Warm the speech engines NOW, at radio start — not at the first ring.
    //
    // ⚠️ Loading a sherpa/VITS voice bundle takes ~1.8s, but the ring window
    // is only ~1.3-1.7s (auto-answer holds for the caller id + the
    // personalization overlay, then answers). Warming on CallIncoming
    // therefore finished AFTER the call was already answered and the caller
    // sat in silence waiting for it — measured live 2026-07-18: answered at
    // 11:06:43.765, "TTS engine pre-warmed (ring)" at 11:06:45.615. The ring
    // warm below stays as a cheap idempotent safety net (the workers no-op
    // when an engine is already loaded); this makes the FIRST call as fast as
    // every later one. Both sends are to worker threads, so the radio loop
    // never blocks on the load.
    #[cfg(all(target_os = "windows", feature = "voice"))]
    {
        if should_prepare_local_speech(realtime_selected, false) {
            let _ = stt_tx.send(SttWork::Warm);
            synth.warm();
            eprintln!(
                "[aokie-plugin] warming the speech engines at radio start (the first call must not wait for a model load)"
            );
        }
    }

    loop {
        let mut idle = true;
        status.loop_beat.fetch_add(1, Ordering::Relaxed);
        status
            .loop_phase
            .store(loop_phase::EVENTS, Ordering::Relaxed);

        #[cfg(feature = "voice")]
        drain_bluetooth_events(
            bt,
            outbox,
            sink,
            &status,
            &remote_media,
            &mut phantom_ring_count,
            &mut last_phone_persisted,
            &last_phone_path,
            &mut tracker,
            &mut pending_companion_end_caller,
            &mut ctx,
            &mut idle,
            realtime_selected,
            &synth,
            &stt_tx,
            &stt_result_rx,
            &mut stt_buf,
            &mut stt_outstanding,
            &mut spec_utterance,
            &mut stale_specs,
            &mut stt_had_speech,
            &mut stt_silence,
            agent_enabled,
            &agent_endpoint,
            &agent_persona,
            &agent_model,
            &mut agent_client,
            &pending_agent_client,
            audio_transcript,
            audio_capture,
            &heard_tx,
            &transcript_client_cache,
            &mut utt_audio,
            agent_hangup,
        );
        #[cfg(not(feature = "voice"))]
        drain_bluetooth_events(
            bt,
            outbox,
            sink,
            &status,
            &remote_media,
            &mut phantom_ring_count,
            &mut last_phone_persisted,
            &last_phone_path,
            &mut tracker,
            &mut pending_companion_end_caller,
            &mut ctx,
            &mut idle,
        );

        // Auto-connect the last phone once the dongle is initialised and no
        // phone is on the line yet. bt.connect() blocks up to its stall
        // watchdog (~10s) — the same synchronous path the manual Reconnect
        // control uses — so this is at most one blocking attempt per ~25s.
        if let Some(due) = auto_connect_next {
            if std::time::Instant::now() >= due {
                let connected = status.connected.load(Ordering::Relaxed)
                    || status.connected_address.lock().unwrap().is_some();
                if connected {
                    auto_connect_next = None; // a phone is on the line — done
                } else if !status.initialized.load(Ordering::Relaxed) {
                    // Dongle not ready yet — check again shortly.
                    auto_connect_next =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                } else if auto_connect_attempts >= AUTO_CONNECT_MAX_ATTEMPTS {
                    eprintln!(
                        "[aokie-plugin] auto-connect gave up after {AUTO_CONNECT_MAX_ATTEMPTS} attempts — use Reconnect to page the phone"
                    );
                    auto_connect_next = None;
                } else {
                    // Target: the last connected phone if we have one on
                    // record; otherwise ROTATE through the bonded devices
                    // across attempts, so a stale bond that stalls (e.g. a
                    // phone that forgot us) doesn't block the real phone. Once
                    // one connects it's persisted, so later starts page it
                    // directly.
                    let bonded = bt.bonded_devices();
                    let target = last_phone_persisted
                        .clone()
                        .filter(|a| bonded.iter().any(|(b, _)| b.eq_ignore_ascii_case(a)))
                        .or_else(|| {
                            (!bonded.is_empty()).then(|| {
                                bonded[auto_connect_attempts as usize % bonded.len()]
                                    .0
                                    .clone()
                            })
                        });
                    match target {
                        Some(addr) => {
                            auto_connect_attempts += 1;
                            eprintln!(
                                "[aokie-plugin] auto-connecting to last phone {addr} (attempt {auto_connect_attempts}/{AUTO_CONNECT_MAX_ATTEMPTS})"
                            );
                            match bt.connect(&addr) {
                                Ok(true) => {
                                    // Page started — DeviceConnected confirms.
                                    auto_connect_next = Some(
                                        std::time::Instant::now()
                                            + std::time::Duration::from_secs(25),
                                    );
                                }
                                Ok(false) => auto_connect_next = None, // already connected
                                Err(e) => {
                                    eprintln!("[aokie-plugin] auto-connect attempt failed: {e}");
                                    auto_connect_next = Some(
                                        std::time::Instant::now()
                                            + std::time::Duration::from_secs(20),
                                    );
                                }
                            }
                        }
                        None => {
                            eprintln!(
                                "[aokie-plugin] auto-connect: no bonded phone to page — pair one and it will connect on startup"
                            );
                            auto_connect_next = None;
                        }
                    }
                }
            }
        }

        #[cfg(feature = "voice")]
        service_remote_media_transitions(
            bt,
            &status,
            &remote_media,
            &mut tracker,
            &mut ctx,
            &synth,
            &mut stt_buf,
            &mut stt_had_speech,
            &mut stt_silence,
            &mut aec,
        );
        #[cfg(not(feature = "voice"))]
        service_remote_media_transitions(
            bt,
            &status,
            &remote_media,
            &mut tracker,
            &mut ctx,
        );

        #[cfg(feature = "voice")]
        {
            let remote = remote_media.snapshot();
            if remote.service_mode == crate::remote_media::ServiceMode::ConsultActive {
                if consult_started_for.is_none() {
                    let started = remote_media.active_consult_binding().and_then(|binding| {
                        let request_id = ctx.pending_assistance.as_ref()?.request_id.clone();
                        let request = crate::assistance::global().voice_request(&request_id)?;
                        let worker = private_consult.as_ref()?;
                        worker
                            .start(request, binding.device_id)
                            .map(|()| request_id)
                            .ok()
                    });
                    if let Some(request_id) = started {
                        consult_started_for = Some(request_id);
                    } else {
                        let _ = remote_media.end_active_consult("consult_pipeline_unavailable");
                    }
                }
            } else {
                consult_started_for = None;
            }
        }

        // The only remote caller-bound lane. Both the media worker and this
        // pop validate callEpoch/ownerEpoch/lease/fence independently.
        // Consult audio lives on a distinct queue and can never arrive here.
        let remote_tx_rate = bt.get_sample_rate() as u32;
        if remote_tx_rate > 0 {
            let talk_binding = remote_media.active_talk_binding();
            for _ in 0..24 {
                let Some(frame) = remote_media.try_recv_talk_pcm() else {
                    break;
                };
                let pcm = crate::remote_media::resample_mono(
                    &frame.samples,
                    frame.sample_rate,
                    remote_tx_rate,
                );
                if !pcm.is_empty() {
                    if let Some(binding) = talk_binding.as_ref() {
                        // Explicitly caller-owned TX; this is the one path
                        // that intentionally bypasses Aokie/TTS suppression.
                        // The final Bluetooth write is linearized under the
                        // exact Desktop mute/lease gate, so an acknowledged
                        // mute cannot race one already-popped PCM frame.
                        let sent = remote_media.send_talk_pcm_if_unmuted(binding, &pcm, || {
                            crate::backend::RadioBackend::send_audio(bt, &pcm)
                        });
                        if sent {
                            remote_media.try_mirror_companion_output(&pcm, remote_tx_rate);
                        }
                    }
                }
            }
        }

        #[cfg(feature = "voice")]
        reconcile_switchboard(
            bt,
            outbox,
            sink,
            &control_rx,
            &status,
            &remote_media,
            &mut tracker,
            &mut pending_companion_end_caller,
            &mut ctx,
            &mut parked,
            &mut pending_ctx_restore,
            &mut prev_call_held,
            &mut promote_greet_for,
            &mut resume_line_for,
            &mut pending_controls,
            &synth,
            &stt_current_gen,
            &probe_result_rx,
            &mut stt_buf,
            &mut stt_had_speech,
            &mut stt_silence,
            &screen_policy,
            auto_hold,
            &mut auto_hold_done_for,
            protected_max_ms,
        );
        #[cfg(not(feature = "voice"))]
        reconcile_switchboard(
            bt,
            outbox,
            sink,
            &control_rx,
            &status,
            &remote_media,
            &mut tracker,
            &mut pending_companion_end_caller,
            &mut ctx,
            &mut parked,
            &mut pending_ctx_restore,
            &mut prev_call_held,
            &mut promote_greet_for,
            &mut resume_line_for,
            &mut pending_controls,
        );

        // ── Phase 4 AUTO-HOLD juggle (v2, VERIFIED): a second caller knocked
        // mid-call. Only the ACTIVE call hears Aokie, so the receptionist
        // juggles the audio path: tell the current caller it will be a
        // moment → CHLD=2 → ask the newcomer to hold with their queue
        // position → CHLD=2 → resume the current caller. The newcomer ends
        // up PARKED and is greeted when the current call finishes (the
        // reconciliation block above auto-retrieves them).
        //
        // v2 after the z49 live incident (the second of two rapid swaps tore
        // both calls down while the code assumed success): every CHLD
        // outcome is now VERIFIED against the phone's own reporting (the
        // callheld indicator + an on-demand AT+CLCC) BEFORE session state
        // moves, the toggles are separated by an enforced settle dwell, and
        // every failure shape converges to a safe single-call state instead
        // of speaking into the void. Runs once per waiting caller.
        #[cfg(all(target_os = "windows", feature = "voice"))]
        run_auto_hold_juggle(
            bt,
            outbox,
            sink,
            &control_rx,
            &status,
            &remote_media,
            &synth,
            &stt_current_gen,
            &probe_result_rx,
            &mut stt_buf,
            &mut stt_had_speech,
            &mut stt_silence,
            &mut pending_controls,
            &screen_policy,
            auto_hold,
            &mut auto_hold_done_for,
            &mut promote_greet_for,
            &mut resume_line_for,
            &mut aec,
            protected_max_ms,
            &mut tracker,
            &mut voice_call_gen,
            &mut realtime_lane,
            &mut realtime_resume_call,
            &mut ctx,
            &mut parked,
            &mut pending_ctx_restore,
            &mut prev_call_held,
        );

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
            if let Some(old) = realtime_lane.take() {
                if let Some(item_id) = old.output_pacer.active_item() {
                    let _ = old
                        .session
                        .cancel_output(item_id, old.output_pacer.audible_played_ms(Instant::now()));
                }
                old.session.stop("call generation changed");
                bt.flush_tx_audio();
            }
            realtime_legacy_call = None;
            realtime_failed_call = None;
            realtime_resume_call = None;
            realtime_answered_at = None;
            realtime_midcall_failure = None;
            realtime_terminal_call = None;
            realtime_deferred_policy_failure = None;
            status.realtime_ready.store(false, Ordering::Relaxed);
            // A turn still held open when its call ends (hangup inside the
            // continuation window) is RECORDED against that call — losing the
            // caller's last fragment (often the tail of a phone number) is
            // worse than a late turn event — but never answered.
            if let Some(p) = ctx.pending_turn.take() {
                if !p.corr.is_empty() && !p.text.is_empty() {
                    // A turn spoken while the PIN gate was armed IS the PIN —
                    // redact it even on the end-of-call flush.
                    let pin_turn = ctx.manager_gate.awaiting_pin;
                    let recorded = if pin_turn {
                        "[manager PIN redacted]"
                    } else {
                        p.text.as_str()
                    };
                    eprintln!(
                        "[aokie-plugin] flushing held caller turn from ended call: {}",
                        content_for_log(recorded)
                    );
                    emit_turn(outbox, sink, &p.corr, ctx.turn_index, "caller", recorded);
                    if !pin_turn {
                        let setting = ctx
                            .call_agent_overlay
                            .as_ref()
                            .and_then(|overlay| overlay.persona.as_deref())
                            .unwrap_or(&agent_persona);
                        maybe_spawn_transcript_correction(
                            audio_transcript,
                            &p.corr,
                            ctx.turn_index,
                            &p.text,
                            &p.audio,
                            ctx.prev_heard.as_ref(),
                            &ctx.history,
                            setting,
                            agent_client
                                .clone()
                                .or_else(|| pending_agent_client.lock().unwrap().clone()),
                            &transcript_client_cache,
                            &heard_tx,
                        );
                    }
                }
            }
            voice_call_gen = tracker.generation();
            stt_current_gen.store(voice_call_gen, Ordering::Relaxed);
            // Phase 4 isolation: the ENTIRE per-caller conversational state
            // swaps for a fresh CallVoiceContext in one move — reset-by-
            // construction, no field can be forgotten here again (the class
            // that produced the §9.3 overlay leak and the pace leak). The
            // switchboard slice will PARK the outgoing context per caller
            // instead of dropping it.
            let fresh_lane = tracker.call_id().map(|id| {
                crate::realtime::RealtimeLane::new(id.to_string(), voice_call_gen, Instant::now())
            });
            // Phase 4: a RESUME installs the parked caller's context whole
            // (conversation, PIN gate, pace — everything travels with them);
            // otherwise a fresh one. Either way the captions lane belongs to
            // the new call EPOCH, never a parked one.
            let incoming_ctx = pending_ctx_restore
                .take()
                .unwrap_or_else(|| CallVoiceContext::fresh(None));
            let prev_ctx = std::mem::replace(&mut ctx, incoming_ctx);
            ctx.rt_lane = fresh_lane;
            // §9.3: the call-scoped agent overlay dies WITH its call — the
            // next caller can never inherit the previous caller's persona.
            // EXCEPT an overlay already bound to THIS (new) call: a
            // plugin-dialed outbound call sets its opening-line/purpose
            // overlay at DIAL time, one loop pass before this reset sees the
            // generation change (live bug 2026-07-14, first outbound test
            // call 2821e7e2: an unconditional wipe threw the overlay away
            // and the agent greeted the callee with the INBOUND greeting,
            // knowing nothing about the call it had just placed).
            if prev_ctx
                .call_agent_overlay
                .as_ref()
                .map(|o| o.call_id.as_str())
                == tracker.call_id()
            {
                ctx.call_agent_overlay = prev_ctx.call_agent_overlay;
            }
            if ctx.desktop_realtime_responder {
                realtime_resume_call = tracker
                    .current()
                    .filter(|call| call.is_active() && (!call.outbound || call.agent_owned))
                    .map(|call| call.id.clone());
            }
            if let Some(lane) = ctx.rt_lane.as_mut() {
                if let Some(line) = lane.phase("listening", Instant::now()) {
                    let _ = sink.send_line(&line);
                }
            }
            // §9.2: no caller turns exist yet in the new call — any reply
            // still naming the previous call's turn number must read stale.
            status.last_caller_turn.store(0, Ordering::Relaxed);
            synth.reset_call();
            while probe_result_rx.try_recv().is_ok() {}
            let _ = stt_tx.send(SttWork::ResetCall);
            // Self-healing (AOK-LIF-002): skipped/empty STT jobs never send a
            // result, so the in-flight counter resets at every call boundary
            // rather than accumulating drift across calls.
            stt_outstanding = 0;
            // Hardware-transient state (the LINE's, not the caller's) is
            // destroyed at every boundary — never parked, never restored.
            utt_audio.clear();
            stt_buf.clear();
            stt_had_speech = false;
            stt_silence = Duration::ZERO;
            mute_stt_until = None;
            turn_overlapped = false;
            turn_overlap_at = None;
            spec_utterance = None;
            stale_specs.clear();
            greet_hold_started = None;
            answer_hold_started = None;
            live_hyp = None;
            live_hyp_prev = None;
            live_hyp_shipped = 0;
            live_hyp_at = None;
            live_probe_in_flight = false;
            if let Some(sp) = spec_reply.take() {
                sp.cancel.store(true, Ordering::Relaxed);
                status.spec_llm_wasted.fetch_add(1, Ordering::Relaxed);
            }
            // Drop the echo canceller entirely rather than just resetting its
            // FIFOs (review sweep): it was built at the FIRST call's SCO rate
            // and reset() keeps that rate + filter length. Back-to-back calls
            // can negotiate different codecs (mSBC 16 kHz vs CVSD 8 kHz), so a
            // reused AEC would run at the wrong rate and cancel nothing. None
            // makes it rebuild at THIS call's actual sample rate on the first
            // captured frame below.
            aec = None;
        }

        // Phase 4: a caller retrieved from hold mid-conversation hears the
        // resume line the moment their audio path is back (the promoted
        // greeting covers never-greeted callers; this covers everyone else —
        // a SILENT return was the live complaint on the first supervised
        // switchboard test). Retries across passes while the SCO
        // re-establishes; expires quietly if the call moves on.
        #[cfg(feature = "voice")]
        if let Some((id, set_at)) = resume_line_for.clone() {
            if tracker.call_id() != Some(id.as_str())
                || set_at.elapsed() > std::time::Duration::from_secs(10)
            {
                resume_line_for = None;
            } else if !should_speak_legacy_resume(realtime_selected, ctx.desktop_realtime_responder)
            {
                // The parked caller's WebSocket is intentionally disposable.
                // Suppress the legacy local-TTS resume announcement; one fresh
                // Realtime session/greeting owns the restored audio lane.
                resume_line_for = None;
                if let Some(lane) = realtime_lane.take() {
                    if let Some(item_id) = lane.output_pacer.active_item() {
                        let _ = lane.session.cancel_output(
                            item_id,
                            lane.output_pacer.audible_played_ms(Instant::now()),
                        );
                    }
                    lane.session.stop("parked realtime caller is resuming");
                    bt.flush_tx_audio();
                }
                aec = None;
                status.realtime_ready.store(false, Ordering::Relaxed);
                realtime_resume_call = Some(id);
            } else if tracker.current().is_some_and(|s| s.is_active()) {
                let sr = bt.get_sample_rate();
                if sr > 0 {
                    resume_line_for = None;
                    let _ = speak_announcement(
                        bt,
                        &synth,
                        HOLD_PRIMARY_RESUME_LINE,
                        sr,
                        &ctx.pace,
                        protected_max_ms,
                        &control_rx,
                        &mut pending_controls,
                    );
                    emit_turn(
                        outbox,
                        sink,
                        &id,
                        ctx.turn_index,
                        "bot",
                        HOLD_PRIMARY_RESUME_LINE,
                    );
                    ctx.turn_index += 1;
                }
            }
        }

        status
            .loop_phase
            .store(loop_phase::CALL_SETUP, Ordering::Relaxed);
        // Flush a buffered incoming call once the caller id is known or the
        // grace window elapses.
        let flush_incoming = tracker.current().is_some_and(|s| {
            s.incoming_pending() && (s.caller_id.is_some() || s.incoming_pending_ms() > 800)
        });
        if flush_incoming {
            flush_incoming_if_pending(&mut tracker, outbox, sink);
        }

        // Phase 2 dial watchdog: an agent-placed outbound attempt that is
        // still not answered after 60s is abandoned — the carrier/phone
        // usually times MO attempts out themselves, but a stuck attempt must
        // never hold the line (and the receptionist's availability) forever.
        // CHUP once; the CIEV stream then terminates the session with the
        // honest outcome (no_answer/failed, reason "cancelled").
        if let Some(s) = tracker.current() {
            if s.outbound
                && s.agent_owned
                && !s.is_active()
                && s.ringing_for_ms() > 60_000
                && dial_cancel_sent.as_deref() != Some(s.id.as_str())
            {
                eprintln!(
                    "[aokie-plugin] outbound attempt {} unanswered after 60s — cancelling (AT+CHUP)",
                    s.id
                );
                dial_cancel_sent = Some(s.id.clone());
                tracker.note_intent(crate::call_session::TerminationIntent::AgentHangup);
                if let Err(e) = bt.hangup() {
                    eprintln!("[aokie-plugin] outbound cancel failed: {e}");
                }
            }
        }

        // Dead-air watchdog: an answered call whose audio channel remains
        // absent for one continuous bounded window is silence the caller can
        // do nothing about. A transient late-call bounce starts a new window;
        // total call age is irrelevant. Companion ownership is unwound first
        // and receives a complete fresh Aokie recovery window. Suppressed
        // around our own CHLD switches, where SCO legitimately bounces.
        {
            let switch_recent = status
                .switch_in_flight
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < std::time::Duration::from_secs(10));
            let active_call_id = tracker
                .current()
                .filter(|session| session.is_active())
                .map(|session| session.id.clone());
            let aokie_owner = remote_media.aokie_owner_fence();
            // The mutex-backed owner proof wins over the cached atomic if a
            // refresh is crossing this exact tick.
            let remote_reserved = aokie_owner.is_none() && remote_media.radio_reserved();
            // The no-SCO watchdog is a WinUSB-SCO safety net: on the native
            // Windows-stack transport the call audio path is Windows-owned
            // (and may legitimately sit outside our WASAPI pump), so ending
            // the call there is wrong — the transport opts out.
            let dead_air_action = if bt.sco_dead_air_watchdog() {
                no_sco_watchdog.check(
                    active_call_id.as_deref(),
                    bt.get_sample_rate() > 0,
                    switch_recent,
                    aokie_owner.is_some(),
                    remote_reserved,
                    std::time::Instant::now(),
                )
            } else {
                None
            };
            match dead_air_action {
                Some(NoScoAction::RequestRemoteReturn) => {
                    eprintln!(
                        "[aokie-plugin] call audio channel lost during Companion ownership - returning the caller to Aokie before the hardware watchdog may act"
                    );
                    remote_media.fail_closed_all("sco_unavailable");
                }
                Some(NoScoAction::NudgeCodecConnection) => {
                    let call_id = active_call_id.as_deref().unwrap_or("unknown");
                    match bt.codec_connect() {
                        Ok(()) => eprintln!(
                            "[aokie-plugin] call {call_id} has no audio channel - sent AT+BCC so the phone re-establishes SCO (self-heal before the dead-air hangup)"
                        ),
                        Err(error) => eprintln!(
                            "[aokie-plugin] call {call_id} has no audio channel and the AT+BCC self-heal is unavailable: {error}"
                        ),
                    }
                }
                Some(NoScoAction::HangUp) => {
                    let expected = aokie_owner
                        .as_ref()
                        .expect("watchdog only returns HangUp for a proven Aokie owner");
                    let call_id = active_call_id.as_deref().unwrap_or("unknown");
                    eprintln!(
                        "[aokie-plugin] call {call_id} has had NO audio channel continuously for 8s under Aokie ownership - hanging up (dead air beats silence)"
                    );
                    match remote_media.with_aokie_owner(expected, || {
                        tracker.note_intent(crate::call_session::TerminationIntent::DeviceLost);
                        bt.hangup()
                    }) {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            eprintln!("[aokie-plugin] dead-air hangup failed: {error}");
                        }
                        Err(reason) => {
                            eprintln!("[aokie-plugin] dead-air hangup skipped: {reason}");
                            no_sco_watchdog.rearm_after_owner_race(std::time::Instant::now());
                        }
                    }
                }
                None => {}
            }
        }

        // A takeover may legitimately win while the fixed screening message
        // is playing. Never CHUP underneath that human route; retain the
        // disposition and apply it only if the exact same caller later returns
        // to Aokie. A changed/ended physical call retires the pending action.
        #[cfg(feature = "voice")]
        if let Some(screened_call_id) = screened_hangup_pending_for.clone() {
            let same_active_call = tracker
                .current()
                .is_some_and(|session| session.is_active() && session.id == screened_call_id);
            if !same_active_call {
                screened_hangup_pending_for = None;
            } else if let Some(owner) = aokie_owner_for_call(&remote_media, &screened_call_id) {
                eprintln!(
                    "[aokie-plugin] screened caller returned from Companion - applying deferred screen hangup"
                );
                match remote_media.with_aokie_owner(&owner, || {
                    tracker.note_intent(crate::call_session::TerminationIntent::AgentHangup);
                    bt.flush_tx_audio();
                    bt.hangup()
                }) {
                    Ok(Ok(())) => {
                        ctx.agent_hung_up = true;
                        screened_hangup_pending_for = None;
                    }
                    Ok(Err(error)) => {
                        eprintln!("[aokie-plugin] deferred screened-call hangup failed: {error}");
                        ctx.agent_hung_up = true;
                        screened_hangup_pending_for = None;
                    }
                    Err(reason) => eprintln!(
                        "[aokie-plugin] deferred screened-call hangup still waiting: {reason}"
                    ),
                }
            }
        }

        #[cfg(all(target_os = "windows", feature = "voice"))]
        service_realtime_lane(
            bt,
            outbox,
            sink,
            &status,
            &greeting,
            &host_rpc,
            &remote_media,
            realtime_selected,
            &realtime_config,
            &synth,
            &stt_tx,
            &agent_persona,
            &mut answer_hold_started,
            &screen_policy,
            agent_hangup,
            &mut promote_greet_for,
            &mut aec,
            &mut tracker,
            voice_call_gen,
            &mut realtime_lane,
            &mut realtime_legacy_call,
            &mut realtime_failed_call,
            &mut realtime_resume_call,
            &mut realtime_answered_at,
            &mut realtime_midcall_failure,
            &mut realtime_terminal_call,
            &mut realtime_deferred_policy_failure,
            &mut ctx,
        );

        #[cfg(all(target_os = "windows", feature = "voice"))]
        service_realtime_failures(
            bt,
            outbox,
            sink,
            &control_rx,
            &status,
            &remote_media,
            &synth,
            &mut pending_controls,
            &mut tracker,
            &mut realtime_resume_call,
            &mut realtime_answered_at,
            &mut realtime_midcall_failure,
            &mut realtime_terminal_call,
            &mut realtime_deferred_policy_failure,
            &mut ctx,
        );

        // Auto-answer ASAP: the instant a call is present and not yet answered,
        // send the answer â€” before the audio channel comes up and freezes the
        // loop. Answer exactly once per call (the session's auto_answered flag).
        // Never for OUTBOUND sessions (Phase 2): "not yet active" there means
        // the REMOTE side hasn't picked up — an ATA into our own dialing
        // attempt is nonsense.
        if auto_answer {
            if let Some(s) = tracker.current_mut() {
                if !s.auto_answered && !s.is_active() && !s.outbound {
                    // AOK-VOICE-001: never answer into silence. A KNOWN voice
                    // failure (asset preflight or a live engine/synthesis
                    // failure) means the receptionist can't hear or speak —
                    // leave the call ringing for the operator's phone instead
                    // of answering it into a dead line. Fail-safe direction:
                    // only a DEFINITIVE recorded failure blocks; healthy or
                    // not-yet-exercised pipelines answer as before.
                    #[cfg(feature = "voice")]
                    let voice_block: Option<String> = {
                        let realtime_call = realtime_owns_call(
                            realtime_selected,
                            realtime_legacy_call.as_deref(),
                            Some(s.id.as_str()),
                        );
                        if realtime_call {
                            if let Some(error) = realtime_backend_error(
                                true,
                                bt.realtime_call_audio_supported(),
                                bt.backend_name(),
                            ) {
                                Some(error)
                            } else if realtime_lane.as_ref().is_some_and(|lane| {
                                lane.call_id == s.id
                                    && lane.generation == voice_call_gen
                                    && lane.ready
                            }) {
                                None
                            } else {
                                realtime_failed_call
                                    .as_ref()
                                    .filter(|(call_id, _)| call_id == &s.id)
                                    .map(|(_, reason)| reason.clone())
                                    .or_else(|| {
                                        Some(
                                            "Desktop realtime voice is still preparing — the call will keep ringing"
                                                .to_string(),
                                        )
                                    })
                            }
                        } else {
                            let screened = (!s.outbound)
                                .then(|| screen_policy.verdict(s.caller_id.as_deref()))
                                .flatten();
                            let needs_speech = screened
                                .map(|reason| {
                                    screened_call_needs_tts(screen_policy.message_for(reason))
                                })
                                .unwrap_or(true);
                            let needs_hearing_and_reasoning = screened.is_none();
                            let tts = needs_speech
                                .then(|| status.tts_error.lock().unwrap().clone())
                                .flatten();
                            let stt = needs_hearing_and_reasoning
                                .then(|| status.stt_error.lock().unwrap().clone())
                                .flatten();
                            let llm = if agent_enabled && needs_hearing_and_reasoning {
                                status.llm_error.lock().unwrap().clone()
                            } else {
                                None
                            };
                            let self_test = match status.self_test.lock().unwrap().as_ref() {
                                _ if !needs_hearing_and_reasoning => None,
                                None => Some(
                                    "voice self-test still running — arming once it passes"
                                        .to_string(),
                                ),
                                Some(r) if !r.ok => {
                                    Some(format!("voice self-test failed: {}", r.detail))
                                }
                                Some(_) => None,
                            };
                            tts.or(stt).or(llm).or(self_test)
                        }
                    };
                    #[cfg(not(feature = "voice"))]
                    let voice_block: Option<String> = None;
                    if let Some(reason) = voice_block {
                        if voice_block_logged_call.as_deref() != Some(s.id.as_str()) {
                            eprintln!(
                                "[aokie-plugin] auto-answer BLOCKED — voice pipeline down ({reason}); the call rings through to the operator"
                            );
                            voice_block_logged_call = Some(s.id.clone());
                        }
                    } else {
                        // Ring-time personalization window: let +CLIP land and
                        // the personalize flow push its call-scoped overlay
                        // BEFORE picking up — bounded (one ring, not lag), and
                        // the overlay's arrival short-circuits it.
                        #[cfg(feature = "voice")]
                        let (hold, overlay_ready) = {
                            let overlay_ready = ctx
                                .call_agent_overlay
                                .as_ref()
                                .is_some_and(|o| o.call_id == s.id);
                            let started = *answer_hold_started.get_or_insert_with(Instant::now);
                            (
                                hold_auto_answer(
                                    s.caller_id.as_deref().is_some_and(|c| !c.is_empty()),
                                    overlay_ready,
                                    started.elapsed(),
                                ),
                                overlay_ready,
                            )
                        };
                        #[cfg(not(feature = "voice"))]
                        let (hold, overlay_ready) = (false, false);
                        if hold {
                            idle = false;
                        } else {
                            match bt.answer_call() {
                                Ok(()) => {
                                    #[cfg(feature = "voice")]
                                    eprintln!(
                                        "[aokie-plugin] auto-answered incoming call ({}ms ring window{})",
                                        answer_hold_started
                                            .map(|t| t.elapsed().as_millis())
                                            .unwrap_or(0),
                                        if overlay_ready {
                                            ", personalization READY"
                                        } else {
                                            ""
                                        }
                                    );
                                    #[cfg(not(feature = "voice"))]
                                    {
                                        let _ = overlay_ready;
                                        eprintln!(
                                            "[aokie-plugin] auto-answered incoming call (immediate)"
                                        );
                                    }
                                }
                                Err(e) => eprintln!("[aokie-plugin] auto-answer failed: {e}"),
                            }
                            s.auto_answered = true;
                            idle = false;
                        }
                    }
                }
            }
        }

        // Stage-2 diagnostic: once the call's audio channel is up (sample rate
        // becomes non-zero), play a short two-note chime to the caller to verify
        // the OUTBOUND SCO path actually reaches the phone on this dongle. Real
        // TTS speech replaces this once outbound audio is confirmed. Gated by
        // settings.answerTone.
        #[cfg(feature = "voice")]
        if ensure_overlap_capture(&mut aec, barge_in, bt.get_sample_rate()) {
            eprintln!("[aokie-plugin] overlap capture ready (AEC @ {}Hz, interruption threshold {barge_rms}); independent of greeting", bt.get_sample_rate());
        }
        #[cfg(feature = "voice")]
        play_tone_and_greet(
            bt,
            outbox,
            sink,
            &status,
            answer_tone,
            &remote_media,
            &mut tracker,
            &mut ctx,
            &mut idle,
            &control_rx,
            &mut greeting,
            realtime_selected,
            &synth,
            &stt_tx,
            &probe_result_rx,
            &mut stt_buf,
            &mut stt_had_speech,
            &mut stt_silence,
            agent_enabled,
            &mut greet_hold_started,
            &mut screened_hangup_pending_for,
            &mut pending_controls,
            &mut mute_stt_until,
            barge_in,
            &screen_policy,
            &mut promote_greet_for,
            barge_rms,
            &mut aec,
            protected_max_ms,
            &mut turn_overlapped,
            &mut turn_overlap_at,
            voice_call_gen,
            &realtime_legacy_call,
            silence_window,
        );
        #[cfg(not(feature = "voice"))]
        play_tone_and_greet(
            bt,
            outbox,
            sink,
            &status,
            answer_tone,
            &remote_media,
            &mut tracker,
            &mut ctx,
            &mut idle,
        );

        // Poll the volatile typed-help mailbox without ever blocking the
        // radio/audio loop. Once the authorised answer arrives, revalidate
        // the complete call fence and relay only marker-free attributed text.
        #[cfg(all(target_os = "windows", feature = "voice"))]
        service_assistance(
            bt,
            outbox,
            sink,
            &control_rx,
            &status,
            &remote_media,
            &synth,
            stt_had_speech,
            &mut pending_controls,
            barge_in,
            barge_rms,
            &mut aec,
            protected_max_ms,
            &mut tracker,
            &mut ctx,
            &mut idle,
        );

        status.loop_phase.store(loop_phase::MIC, Ordering::Relaxed);
        // Inbound caller audio.
        #[cfg(not(feature = "voice"))]
        while let Some(frame) = bt.try_recv_audio() {
            idle = false;
            remote_media.try_push_sco(&frame.samples, frame.sample_rate as u32);
        }
        // Voice build: energy-VAD segment the caller's speech â†’ ship each finished
        // utterance to the STT worker. ~350 RMS (i16 units) gates speech; ~700 ms
        // of trailing silence ends an utterance; sub-350 ms blips are dropped.
        #[cfg(all(target_os = "windows", feature = "voice"))]
        pump_audio_and_reply(
            bt,
            outbox,
            sink,
            &control_rx,
            &status,
            &host_rpc,
            data_dir,
            &remote_media,
            realtime_selected,
            &synth,
            &stt_tx,
            &stt_result_rx,
            &probe_result_rx,
            &mut stt_buf,
            &mut stt_outstanding,
            &mut spec_utterance,
            &mut stale_specs,
            &mut stt_had_speech,
            &mut stt_silence,
            stt_endpoint,
            agent_enabled,
            &agent_endpoint,
            &agent_persona,
            &agent_model,
            &mut agent_client,
            &mut live_hyp,
            &mut live_hyp_prev,
            &mut live_hyp_shipped,
            &mut live_hyp_at,
            &mut live_probe_in_flight,
            &mut spec_reply,
            &pending_agent_client,
            &mut pending_controls,
            &mut mute_stt_until,
            barge_in,
            send_audio,
            audio_transcript,
            audio_capture,
            &heard_tx,
            &heard_rx,
            &transcript_client_cache,
            &mut screen_policy,
            &mut utt_audio,
            agent_hangup,
            barge_rms,
            &mut aec,
            protected_max_ms,
            &mut turn_overlapped,
            &mut turn_overlap_at,
            &mut tracker,
            &mut pending_companion_end_caller,
            voice_call_gen,
            &mut realtime_lane,
            &mut realtime_legacy_call,
            &mut realtime_midcall_failure,
            &mut realtime_terminal_call,
            &mut realtime_deferred_policy_failure,
            &mut ctx,
            silence_window,
            &mut idle,
        );

        // A `false` from the control servicer = graceful shutdown (forced
        // transcript settlements emitted) — the radio loop ends here.
        #[cfg(feature = "voice")]
        let keep_running = service_controls(
            bt,
            outbox,
            sink,
            &control_rx,
            &status,
            &remote_media,
            &mut greeting,
            &mut tracker,
            &mut pending_companion_end_caller,
            &mut ctx,
            &mut parked,
            &mut pending_ctx_restore,
            &mut promote_greet_for,
            &mut resume_line_for,
            &mut pending_controls,
            &mut idle,
            &synth,
            &stt_tx,
            &mut stt_buf,
            &mut stt_had_speech,
            &mut stt_silence,
            agent_enabled,
            &agent_endpoint,
            &mut agent_persona,
            &mut agent_model,
            &mut agent_client,
            &mut mute_stt_until,
            &mut send_audio,
            &mut audio_transcript,
            &mut screen_policy,
            agent_hangup,
            &mut aec,
            protected_max_ms,
            &mut realtime_lane,
            &mut realtime_resume_call,
        );
        #[cfg(not(feature = "voice"))]
        let keep_running = service_controls(
            bt,
            outbox,
            sink,
            &control_rx,
            &status,
            &remote_media,
            &mut greeting,
            &mut tracker,
            &mut pending_companion_end_caller,
            &mut ctx,
            &mut parked,
            &mut pending_ctx_restore,
            &mut promote_greet_for,
            &mut resume_line_for,
            &mut pending_controls,
            &mut idle,
        );
        if !keep_running {
            return;
        }

        // Corrections normally settle from `heard_rx`; this poll is the hard
        // deadline path for a worker that never reports back.
        emit_ready_transcript_settlements(outbox, sink);
        status.loop_phase.store(loop_phase::TAIL, Ordering::Relaxed);
        if idle {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}
