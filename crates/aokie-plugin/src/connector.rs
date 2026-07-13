//! Plugin state + protocol/command dispatch.
//!
//! Owns the four host-facing methods (`plugin.init`, `plugin.health`,
//! `plugin.shutdown`, `connector.request`) and the MVP connector
//! command surface from AOKIE_PLUGIN_CONTRACT.md §2. Hardware-backed
//! commands return a typed `command_failed` ("not yet wired to
//! hardware") rather than faking success; the dev/mock mode
//! (`FORMLOGIC_DEV_MODE=1`) provides a scripted call lifecycle so
//! FormLogic integration tests can run without a dongle.

use std::path::PathBuf;

use aokie_core::dongle_catalog;
use aokie_core::events::{aokie_event, aokie_turn_event, now_iso8601};
use aokie_core::redact::{validate_sms_body, validate_sms_recipient};
use serde_json::{json, Map, Value};

use crate::config::{ConfigStore, PreferredDongle};
use crate::event_bridge::{emit_event, Sink};
use crate::outbox::Outbox;
use crate::rpc::{self, RpcMessage};

/// The only connector this plugin serves.
pub const CONNECTOR_ID: &str = "aokie";

/// Outbox DB file name inside the plugin data dir.
pub const OUTBOX_FILE: &str = "outbox.sqlite";

const RECEPTIONIST_CONFIG_KEYS: [&str; 7] = [
    "persona",
    "greeting",
    "ttsVoice",
    "aiModel",
    "aiEndpoint",
    "sttEndpoint",
    "ttsEndpoint",
];

/// Settings whose values are URLs that will receive caller audio/transcripts —
/// classified via `aokie_core::url_classification` before persisting
/// (audit PRIV-001/C-16).
const ENDPOINT_SETTING_KEYS: &[&str] = &["aiEndpoint", "sttEndpoint", "ttsEndpoint"];

/// Typed connector-level error, surfaced as a JSON-RPC error with
/// `error.data = {code, message}` (connector-response.schema.json
/// codes; the plugin produces `command_failed`, `stale_call` and — for
/// a mis-routed connector id — `connector_missing`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdError {
    pub code: &'static str,
    pub message: String,
}

impl CmdError {
    pub fn failed(message: impl Into<String>) -> Self {
        CmdError {
            code: crate::contract::errors::COMMAND_FAILED,
            message: message.into(),
        }
    }

    /// A call-control command named a `callId` that is not the current
    /// call — the phone was NOT touched (contract `stale_call`; audit
    /// C-01: a stale browser tab must never control a newer call).
    pub fn stale_call(message: impl Into<String>) -> Self {
        CmdError {
            code: crate::contract::errors::STALE_CALL,
            message: message.into(),
        }
    }
}

/// Mock call lifecycle state (dev mode / simulated calls only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockCallState {
    Incoming,
    Active,
    Ended,
}

impl MockCallState {
    fn as_str(self) -> &'static str {
        match self {
            MockCallState::Incoming => "incoming",
            MockCallState::Active => "active",
            MockCallState::Ended => "ended",
        }
    }

    /// The canonical wire state (`contract::call_state`): the mock's
    /// internal "incoming" is the contract's "ringing".
    fn canonical(self) -> &'static str {
        match self {
            MockCallState::Incoming => crate::contract::call_state::RINGING,
            MockCallState::Active => crate::contract::call_state::ACTIVE,
            MockCallState::Ended => crate::contract::call_state::ENDED,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MockCall {
    pub correlation_id: String,
    pub caller: String,
    pub state: MockCallState,
    pub started_at: String,
    pub turns: u32,
}

#[derive(Debug, Clone)]
pub struct MockSmsMessage {
    pub id: String,
    pub direction: &'static str, // "in" | "out"
    pub body: String,
    pub at: String,
}

#[derive(Debug, Clone)]
pub struct MockSmsThread {
    pub id: String,
    pub phone: String,
    pub messages: Vec<MockSmsMessage>,
}

/// In-memory mock stores backing the phone/call/sms commands until
/// the real radio stack lands in aokie-core.
#[derive(Default)]
pub struct MockState {
    pub current_call: Option<MockCall>,
    pub sms_threads: Vec<MockSmsThread>,
}

impl MockState {
    fn thread_for(&mut self, phone: &str) -> &mut MockSmsThread {
        if let Some(idx) = self.sms_threads.iter().position(|t| t.phone == phone) {
            return &mut self.sms_threads[idx];
        }
        self.sms_threads.push(MockSmsThread {
            id: format!("thread_{}", self.sms_threads.len() + 1),
            phone: phone.to_string(),
            messages: Vec::new(),
        });
        self.sms_threads.last_mut().unwrap()
    }
}

/// The whole plugin process state.
pub struct Plugin {
    pub dev_mode: bool,
    pub data_dir: PathBuf,
    pub store: ConfigStore,
    pub outbox: Outbox,
    pub mock: MockState,
    /// The live Bluetooth radio, present in real (non-dev) mode once a
    /// dongle is available. `None` = dev/mock mode or no radio (e.g. a
    /// non-Windows build). When `Some`, the `call.* / phone.* / sms.*`
    /// handlers drive the real radio instead of the scripted mock.
    pub radio: Option<crate::radio::RadioHandle>,
    /// Why the last real-mode radio start failed (FL-CONN-001) — surfaced in
    /// command errors, phone.status and plugin.health so an outage is
    /// diagnosable instead of silently degrading to mock behaviour.
    pub radio_start_error: Option<String>,
    pub initialized: bool,
    pub shutdown_requested: bool,
    /// The host advertised `eventAck` at init: outboxed events await an
    /// `event.ack` before counting as delivered (audit INT-003).
    pub ack_mode: bool,
    /// Replay-thread heartbeat (audit AOK-OUTBOX-002) — None until ack mode
    /// starts the thread; 0 = failed to start; stale = stalled/dead thread.
    pub replay_heartbeat: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// AOK-CONSENT-001: set when the radio was NOT started because consent
    /// enforcement denied the `bluetooth` scope. Surfaced in phone.status,
    /// plugin.health and command errors so an operator sees WHY the phone is
    /// offline (vs a hardware fault). Cleared once consent is satisfied.
    pub consent_blocked: Option<String>,
}

impl Plugin {
    /// Build from the env Desktop provides at spawn. The outbox opens
    /// eagerly — a plugin that can't persist essential events must
    /// fail the handshake, not lose records later.
    pub fn new(dev_mode: bool, data_dir: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("cannot create data dir {}: {e}", data_dir.display()))?;
        let store = ConfigStore::load(&data_dir);
        let outbox = Outbox::open(&data_dir.join(OUTBOX_FILE))
            .map_err(|e| format!("cannot open outbox: {e}"))?;
        Ok(Plugin {
            dev_mode,
            data_dir,
            store,
            outbox,
            mock: MockState::default(),
            radio: None,
            radio_start_error: None,
            initialized: false,
            shutdown_requested: false,
            ack_mode: false,
            replay_heartbeat: None,
            consent_blocked: None,
        })
    }

    /// Ephemeral instance: unique temp data dir + in-memory outbox.
    /// For unit/contract tests only — the production loop always
    /// goes through [`Plugin::new`] with the Desktop-provided dir.
    pub fn ephemeral(dev_mode: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "aokie-plugin-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create ephemeral data dir");
        Plugin {
            dev_mode,
            data_dir: dir.clone(),
            store: ConfigStore::load(&dir),
            outbox: Outbox::open_in_memory().expect("in-memory outbox"),
            mock: MockState::default(),
            radio: None,
            radio_start_error: None,
            initialized: false,
            shutdown_requested: false,
            ack_mode: false,
            replay_heartbeat: None,
            consent_blocked: None,
        }
    }

    /// Best-effort: bring the live radio up in real (non-dev) mode so the
    /// dongle broadcasts "Aokie AI Assistant" and incoming calls flow
    /// through. Idempotent — a running radio is left alone; dev mode keeps
    /// the scripted mock path; a start failure (no dongle / driver not
    /// bound / non-Windows) is logged and simply leaves `radio = None`.
    pub fn ensure_radio_started(&mut self) {
        // Unit tests run on developer/CI machines that may have a real dongle
        // attached; never grab hardware from a `cargo test` process. Live
        // behaviour is verified by the E2E harness, not the unit suite.
        if cfg!(test) || self.dev_mode || self.radio.is_some() {
            return;
        }

        // AOK-CONSENT-001: the radio is the entry point to ALL sensitive
        // processing (pairing, call audio, STT, MAP/PBAP). Gate its start on
        // the `bluetooth` consent scope. In `enforce` a missing / stale /
        // unscoped grant blocks bring-up (recorded in `consent_blocked`, so
        // the phone reads "offline: consent required", not a hardware fault);
        // in the default `warn` posture it only logs + degrades health so an
        // already-deployed receptionist keeps working through the upgrade.
        match self.consent_gate(crate::consent::Scope::Bluetooth) {
            crate::consent::ConsentDecision::Deny(reason) => {
                eprintln!("[aokie-plugin] radio NOT started — consent required: {reason}");
                self.consent_blocked = Some(reason);
                return;
            }
            crate::consent::ConsentDecision::Warn(reason) => {
                eprintln!(
                    "[aokie-plugin] ⚠ consent not recorded: {reason} (consentMode=warn; set \
                     consentMode=enforce to block sensitive processing until consent is given)"
                );
                self.consent_blocked = None;
            }
            crate::consent::ConsentDecision::Allow => {
                self.consent_blocked = None;
            }
        }
        // CONSENT-001 transcription gate: `bluetooth` may be granted while
        // `transcription` is denied — calls still connect, but NO caller
        // audio may reach an STT engine. The radio's STT worker honours
        // AOKIE_STT_DISABLED at startup and drops every frame before an
        // engine (in-process or HTTP) could see it.
        match self.consent_gate(crate::consent::Scope::Transcription) {
            crate::consent::ConsentDecision::Deny(reason) => {
                std::env::set_var("AOKIE_STT_DISABLED", "1");
                eprintln!("[aokie-plugin] transcription consent denied — STT disabled: {reason}");
            }
            _ => {
                std::env::set_var("AOKIE_STT_DISABLED", "0");
            }
        }
        // HFP-codec override (settings.hfpCodec: "auto" | "cvsd" | "wbs").
        // Some dongles (e.g. Broadcom BCM20702) need CVSD-only — mSBC's SCO
        // path is non-functional on them. The aokie_radio runtime reads the
        // AOKIE_HFP_CODEC env var; set it (in-process, before the radio thread
        // spawns) from the setting so the desktop-spawned plugin honours it
        // without the operator having to set an env var.
        if let Some(codec) = self
            .store
            .config
            .settings
            .get("hfpCodec")
            .and_then(|v| v.as_str())
        {
            let codec = codec.trim().to_ascii_lowercase();
            if !codec.is_empty() && codec != "auto" {
                std::env::set_var("AOKIE_HFP_CODEC", &codec);
                eprintln!("[aokie-plugin] hfpCodec setting → AOKIE_HFP_CODEC={codec}");
            }
        }

        // End-of-utterance silence (ms) the STT waits before treating the caller's
        // turn as finished — lower = snappier replies. Tunable via the
        // `sttEndpointMs` setting (150–2000; default 450). The radio reads
        // AOKIE_STT_ENDPOINT_MS at startup.
        if let Some(ms) = self
            .store
            .config
            .settings
            .get("sttEndpointMs")
            .and_then(|v| v.as_u64())
        {
            std::env::set_var("AOKIE_STT_ENDPOINT_MS", ms.to_string());
            eprintln!("[aokie-plugin] sttEndpointMs setting → AOKIE_STT_ENDPOINT_MS={ms}");
        }

        // AOK-CTRL-001: call-level max-silence window (seconds of MUTUAL
        // silence before the agent checks in, then hangs up after a second
        // silent window). 0 disables; the radio clamps non-zero values to
        // 10–600 and defaults to 30 when unset.
        if let Some(secs) = self
            .store
            .config
            .settings
            .get("maxSilenceSecs")
            .and_then(|v| v.as_u64())
        {
            std::env::set_var("AOKIE_MAX_SILENCE_SECS", secs.to_string());
            eprintln!("[aokie-plugin] maxSilenceSecs setting → AOKIE_MAX_SILENCE_SECS={secs}");
        }

        // In-plugin real-time voice agent: when `aiReceptionist` is truthy, the
        // plugin streams the local LLM + speaks the reply itself (low latency)
        // instead of routing through a flow. `aiEndpoint` pins the LLM URL (else
        // it probes llama.cpp :8080 then ollama :11434); `persona` is the system
        // prompt. The radio reads these envs at startup.
        let ai_receptionist = self
            .store
            .config
            .settings
            .get("aiReceptionist")
            .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
            .unwrap_or(false);
        if ai_receptionist {
            std::env::set_var("AOKIE_AI_RECEPTIONIST", "1");
            eprintln!("[aokie-plugin] aiReceptionist ON → in-plugin streaming agent");
        }
        apply_endpoint_env_from_settings(
            &self.store.config.settings,
            "aiEndpoint",
            "AOKIE_AI_ENDPOINT",
        );
        // Optional HTTP speech services. When set, the voice receptionist tries
        // these before loading its in-process Parakeet / Pocket-TTS engines.
        apply_endpoint_env_from_settings(
            &self.store.config.settings,
            "sttEndpoint",
            "AOKIE_STT_ENDPOINT",
        );
        apply_endpoint_env_from_settings(
            &self.store.config.settings,
            "ttsEndpoint",
            "AOKIE_TTS_ENDPOINT",
        );
        // LLM model (`aiModel`); empty = auto-detect the desktop's loaded model.
        if let Some(m) = self
            .store
            .config
            .settings
            .get("aiModel")
            .and_then(|v| v.as_str())
        {
            if !m.trim().is_empty() {
                std::env::set_var("AOKIE_AI_MODEL", m.trim());
                eprintln!(
                    "[aokie-plugin] aiModel setting → AOKIE_AI_MODEL={}",
                    m.trim()
                );
            }
        }
        if let Some(p) = self
            .store
            .config
            .settings
            .get("persona")
            .and_then(|v| v.as_str())
        {
            if !p.trim().is_empty() {
                std::env::set_var("AOKIE_AI_PERSONA", p.trim());
            }
        }
        // TTS voice (pocket-tts predefined: alba/azelma/cosette/eponine/fantine/
        // javert/jean/marius, or a .wav path to clone). Empty = bundle default.
        if let Some(v) = self
            .store
            .config
            .settings
            .get("ttsVoice")
            .and_then(|v| v.as_str())
        {
            if !v.trim().is_empty() {
                std::env::set_var("AOKIE_TTS_VOICE", v.trim());
                eprintln!(
                    "[aokie-plugin] ttsVoice setting → AOKIE_TTS_VOICE={}",
                    v.trim()
                );
            }
        }
        // Full-duplex / barge-in: when `bargeIn` is truthy AND the agent is on,
        // Aokie keeps listening while it speaks (speexdsp echo-cancels its own
        // TTS from the mic) so the caller can talk over it and cut it short.
        // Off by default → the proven half-duplex mute path stays the norm.
        // `bargeSensitivity` tunes the cleaned-mic RMS above which the caller
        // counts as interrupting (lower = easier to interrupt; default 650).
        let barge_in = self
            .store
            .config
            .settings
            .get("bargeIn")
            .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
            .unwrap_or(false);
        if barge_in {
            std::env::set_var("AOKIE_BARGE_IN", "1");
            eprintln!("[aokie-plugin] bargeIn ON → full-duplex (caller can talk over Aokie)");
        }
        // Agent-initiated hangup: when `agentHangup` is truthy AND the agent is on,
        // the receptionist ends the call itself after a completed conversation
        // (says goodbye, then AT+CHUP) so the caller doesn't have to hang up first.
        let agent_hangup = self
            .store
            .config
            .settings
            .get("agentHangup")
            .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
            .unwrap_or(false);
        if agent_hangup {
            std::env::set_var("AOKIE_AGENT_HANGUP", "1");
            eprintln!("[aokie-plugin] agentHangup ON → the agent hangs up when the call is done");
        }
        if let Some(rms) = self
            .store
            .config
            .settings
            .get("bargeSensitivity")
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .filter(|&v| v > 0.0)
        {
            std::env::set_var("AOKIE_BARGE_RMS", (rms as f32).to_string());
            eprintln!("[aokie-plugin] bargeSensitivity setting → AOKIE_BARGE_RMS={rms}");
        }
        // PAIR-001: legacy fixed-PIN ("0000") pairing is OFF unless the
        // operator deliberately enabled the compat setting. The radio warns
        // loudly whenever it's on — a fixed PIN authenticates nothing.
        let legacy_pin = self
            .store
            .config
            .settings
            .get("legacyPairingPin")
            .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
            .unwrap_or(false);
        std::env::set_var(
            "AOKIE_LEGACY_PAIRING_PIN",
            if legacy_pin { "1" } else { "0" },
        );
        if legacy_pin {
            eprintln!(
                "[aokie-plugin] ⚠️ legacyPairingPin ON — fixed-PIN (0000) pairing enabled for \
                 pre-SSP devices; turn it back off once the device is bonded"
            );
        }

        // Auto-answer defaults OFF (audit INT-006/C-15): a receptionist must
        // be explicitly enabled (settings.autoAnswer: true — the pack's
        // Receptionist Settings/configure flow sets it), never assumed. A
        // build with no voice output can never auto-answer — it would answer
        // the caller into silence.
        let mut auto_answer = auto_answer_from_settings(&self.store.config.settings);
        if auto_answer && !cfg!(feature = "voice") {
            eprintln!(
                "[aokie-plugin] autoAnswer disabled: this build has no voice output (voice feature not compiled)"
            );
            auto_answer = false;
        }
        if !auto_answer {
            eprintln!("[aokie-plugin] autoAnswer is OFF — calls ring through to the operator");
        }
        // Stage-2 outbound-audio diagnostic: play a chime to the caller on
        // answer to verify the SCO-OUT path reaches the phone on this dongle
        // (settings.answerTone, default off; superseded by real TTS speech).
        let answer_tone = self
            .store
            .config
            .settings
            .get("answerTone")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // Software virtual-replug at startup to fix the post-boot dead-SCO-iso
        // state (some dongles' iso endpoint is dead until re-enumerated). Set
        // settings.reenumerateHwid to the dongle's hardware id
        // (e.g. "USB\\VID_0A5C&PID_21EC"); unset = skip (safe default).
        let reenumerate_hwid = self
            .store
            .config
            .settings
            .get("reenumerateHwid")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        // Spoken greeting the receptionist plays on answer (voice build).
        let greeting = greeting_from_settings(&self.store.config.settings);
        match crate::radio::spawn(
            self.data_dir.clone(),
            None,
            auto_answer,
            answer_tone,
            reenumerate_hwid,
            greeting,
            self.ack_mode,
        ) {
            Ok(handle) => {
                eprintln!(
                    "[aokie-plugin] live radio starting (real mode, auto_answer={auto_answer})"
                );
                // Initial config revision for call records (AOK-CONFIG-002).
                handle
                    .status
                    .config_version
                    .store(self.store.config.config_version, std::sync::atomic::Ordering::Relaxed);
                self.radio = Some(handle);
                self.radio_start_error = None;
            }
            Err(e) => {
                eprintln!("[aokie-plugin] live radio unavailable: {e}");
                self.radio_start_error = Some(e.to_string());
            }
        }
    }

    /// FL-CONN-001: in real (non-dev) mode a missing radio is an OUTAGE — the
    /// dev-mode mock twin must never answer for it. Every `call.* / phone.* /
    /// sms.*` handler calls this before its mock fallthrough, so an
    /// unavailable radio is a typed failure ("command not performed"), not a
    /// fabricated success the browser/flows would record as real.
    fn require_radio_or_dev(&self, command: &str) -> Result<(), CmdError> {
        if self.dev_mode || self.radio.is_some() {
            return Ok(());
        }
        let cause = self
            .radio_start_error
            .as_deref()
            .unwrap_or("no dongle / driver not bound / startup failed");
        Err(CmdError::failed(format!(
            "{command}: the radio is not running ({cause}) — command not performed"
        )))
    }

    /// CONSENT-001: the enforcement posture from the `consentMode` setting.
    /// Production DEFAULT is `enforce`; `warn` is the explicit developer/
    /// beta override. Dev mode never enforces — it never touches real
    /// hardware or a real caller.
    fn consent_mode(&self) -> crate::consent::ConsentMode {
        if self.dev_mode {
            return crate::consent::ConsentMode::Off;
        }
        crate::consent::ConsentMode::from_setting(
            self.store
                .config
                .settings
                .get("consentMode")
                .and_then(Value::as_str),
        )
    }

    /// CONSENT-001: the Desktop-provided Ed25519 verify key for consent
    /// grants (set by the parent Desktop at spawn). Present ⇒ ONLY grants
    /// signed by THIS Desktop install satisfy the gate — the per-install key
    /// is the device binding. Absent (legacy host) ⇒ plain grants accepted.
    fn consent_verify_key(&self) -> Option<String> {
        std::env::var("FORMLOGIC_CONSENT_VERIFY_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// The verified consent record (grant is `None` when nothing VALID is
    /// recorded — missing, unsigned-under-a-signing-desktop, or tampered).
    fn consent_loaded(&self) -> crate::consent::LoadedConsent {
        let key = self.consent_verify_key();
        crate::consent::load_verified(&self.data_dir, key.as_deref())
    }

    /// AOK-CONSENT-001: run the pure consent gate for `scope` against the
    /// verified grant + current mode.
    fn consent_gate(&self, scope: crate::consent::Scope) -> crate::consent::ConsentDecision {
        crate::consent::evaluate(
            self.consent_loaded().grant.as_ref(),
            crate::consent::CURRENT_CONSENT_VERSION,
            self.consent_mode(),
            scope,
            &aokie_core::events::now_iso8601(),
        )
    }

    /// AOK-CONSENT-001: refuse a sensitive command when consent enforcement
    /// denies its scope. A `Deny` (enforce mode) is a typed command failure;
    /// a `Warn` is logged but allowed. Used by the `sms.*` and pairing
    /// handlers (calls/STT are already gated by the radio not starting).
    fn check_consent(&self, command: &str, scope: crate::consent::Scope) -> Result<(), CmdError> {
        match self.consent_gate(scope) {
            crate::consent::ConsentDecision::Deny(reason) => Err(CmdError::failed(format!(
                "{command}: {reason} — enable it in FormLogic (consent required)"
            ))),
            crate::consent::ConsentDecision::Warn(reason) => {
                eprintln!("[aokie-plugin] ⚠ {command}: {reason} (consentMode=warn)");
                Ok(())
            }
            crate::consent::ConsentDecision::Allow => Ok(()),
        }
    }

    /// AOK-CONSENT-001 `consent.get`: the recorded grant (or null), the
    /// required version, the current enforcement mode, and the live gate
    /// decision for the bluetooth scope — so FormLogic can show consent
    /// status and whether sensitive processing is currently permitted.
    fn consent_get(&self) -> Result<Value, CmdError> {
        let loaded = self.consent_loaded();
        let mode = self.consent_mode();
        let decision = crate::consent::evaluate(
            loaded.grant.as_ref(),
            crate::consent::CURRENT_CONSENT_VERSION,
            mode,
            crate::consent::Scope::Bluetooth,
            &aokie_core::events::now_iso8601(),
        );
        Ok(json!({
            "grant": loaded.grant,
            "signed": loaded.signed,
            "note": loaded.note,
            "requiredVersion": crate::consent::CURRENT_CONSENT_VERSION,
            "mode": mode.as_str(),
            "signatureRequired": self.consent_verify_key().is_some(),
            "bluetooth": { "allowed": !decision.is_denied(), "reason": decision.reason() },
            "blocked": self.consent_blocked,
        }))
    }

    /// AOK-CONSENT-001 `consent.set`: record a consent grant issued by
    /// FormLogic (the operator accepted the wizard). `accepted_at` is stamped
    /// by the plugin, not trusted from the wire. Once recorded, the radio is
    /// (re)started if consent had blocked it.
    fn consent_set(&mut self, payload: &Value) -> Result<Value, CmdError> {
        let obj = payload
            .as_object()
            .ok_or_else(|| CmdError::failed("consent.set requires an object body"))?;

        // CONSENT-001 signed path: a Desktop that provisioned a verify key
        // REQUIRES a signed envelope — verify it, then persist the signed
        // bytes VERBATIM (the gate re-verifies from disk on every check, so
        // file tampering breaks the signature).
        if let Some(key) = self.consent_verify_key() {
            let envelope_val = obj.get("envelope").ok_or_else(|| {
                CmdError::failed(
                    "consent.set: this Desktop signs consent grants — send {envelope: <signed grant>} \
                     (issued by the Desktop consent wizard), not a plain grant",
                )
            })?;
            let envelope: crate::consent::SignedConsent =
                serde_json::from_value(envelope_val.clone())
                    .map_err(|e| CmdError::failed(format!("consent.set invalid envelope: {e}")))?;
            let grant = crate::consent::verify_envelope(&envelope, &key)
                .map_err(|e| CmdError::failed(format!("consent.set: {e}")))?;
            crate::consent::save_signed(&self.data_dir, &envelope).map_err(CmdError::failed)?;
            self.consent_blocked = None;
            self.ensure_radio_started();
            return Ok(json!({
                "recorded": true,
                "signed": true,
                "version": grant.version,
                "acceptedAt": grant.accepted_at,
                "expiresAt": grant.expires_at,
                "mode": self.consent_mode().as_str(),
                "radioStarted": self.radio.is_some(),
                "blocked": self.consent_blocked,
            }));
        }

        // Legacy unsigned path (host without a signing key).
        let scopes: crate::consent::ConsentScopes =
            serde_json::from_value(obj.get("scopes").cloned().unwrap_or_else(|| json!({})))
                .map_err(|e| CmdError::failed(format!("consent.set invalid scopes: {e}")))?;
        let version = obj
            .get("version")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(crate::consent::CURRENT_CONSENT_VERSION);
        let accepted_by = obj
            .get("acceptedBy")
            .and_then(Value::as_str)
            .map(str::to_string);
        let signature = obj
            .get("signature")
            .and_then(Value::as_str)
            .map(str::to_string);
        let grant = crate::consent::ConsentGrant {
            version,
            scopes,
            accepted_at: String::new(), // stamped by save()
            accepted_by,
            expires_at: None,
            signature,
        };
        let saved = crate::consent::save(&self.data_dir, grant).map_err(CmdError::failed)?;
        // Consent may now be satisfied — clear the block and try to bring the
        // radio up (idempotent; a no-op if it's already running or unavailable).
        self.consent_blocked = None;
        self.ensure_radio_started();
        Ok(json!({
            "recorded": true,
            "version": saved.version,
            "acceptedAt": saved.accepted_at,
            "mode": self.consent_mode().as_str(),
            "radioStarted": self.radio.is_some(),
            "blocked": self.consent_blocked,
        }))
    }

    /// AOK-CONSENT-001 `consent.revoke`: delete the grant and IMMEDIATELY
    /// stop sensitive work — disable auto-answer and stop the radio — without
    /// a plugin restart. Applies regardless of mode (revoking is an explicit
    /// operator action, not a policy toggle).
    fn consent_revoke(&mut self) -> Result<Value, CmdError> {
        crate::consent::revoke(&self.data_dir).map_err(CmdError::failed)?;
        // Auto-answer off so a still-connected phone can't be answered before
        // the radio is torn down; persisted so it survives a restart too.
        self.store
            .config
            .settings
            .insert("autoAnswer".to_string(), json!(false));
        self.store.config.config_version += 1;
        self.save_config()?;
        let radio_stopped = if let Some(radio) = self.radio.take() {
            let _ = radio.send(crate::radio::RadioControl::Shutdown);
            true
        } else {
            false
        };
        self.consent_blocked = Some("consent was revoked".to_string());
        eprintln!("[aokie-plugin] consent revoked — auto-answer disabled, radio stopped");
        Ok(json!({
            "revoked": true,
            "radioStopped": radio_stopped,
            "autoAnswerDisabled": true,
        }))
    }

    /// Handle one parsed protocol message. Returns the response line
    /// for requests, `None` for notifications (which get no answer).
    pub fn handle_rpc(&mut self, msg: RpcMessage, sink: &mut dyn Sink) -> Option<String> {
        let Some(id) = msg.id.clone() else {
            // Notifications get no answer, but `event.ack` IS processed: the
            // host durably received an outboxed event — mark it delivered
            // (audit INT-003; until then the replay thread keeps re-sending).
            if msg.method == "event.ack" {
                if let Some(key) = msg.params.get("idempotencyKey").and_then(Value::as_str) {
                    if let Err(e) = self.outbox.mark_sent(key) {
                        eprintln!("[aokie-plugin] event.ack for {key} failed to persist: {e}");
                    }
                }
            }
            return None;
        };
        match msg.method.as_str() {
            "plugin.init" => Some(self.handle_init(&id, &msg.params)),
            "plugin.health" => Some(rpc::success_line(&id, self.build_health())),
            "plugin.shutdown" => {
                self.shutdown_requested = true;
                if let Some(radio) = self.radio.as_ref() {
                    let _ = radio.send(crate::radio::RadioControl::Shutdown);
                }
                Some(rpc::success_line(&id, json!({"ok": true})))
            }
            "connector.request" => Some(self.handle_connector_request(&id, &msg.params, sink)),
            _ => Some(rpc::error_line(
                Some(&id),
                rpc::METHOD_NOT_FOUND,
                &format!("method not found: {}", msg.method),
                None,
            )),
        }
    }

    fn handle_init(&mut self, id: &Value, params: &Value) -> String {
        // {desktopVersion, pluginApiVersion, dataDir, devMode} — all
        // advisory except dataDir (re-roots storage) and devMode.
        if let Some(obj) = params.as_object() {
            if let Some(api) = obj.get("pluginApiVersion").and_then(Value::as_i64) {
                if api != 1 {
                    return rpc::error_line(
                        Some(id),
                        rpc::INVALID_PARAMS,
                        &format!("unsupported pluginApiVersion {api} (plugin speaks 1)"),
                        None,
                    );
                }
            }
            if let Some(dir) = obj.get("dataDir").and_then(Value::as_str) {
                let dir = PathBuf::from(dir);
                if dir != self.data_dir {
                    match Self::new(self.dev_mode, dir) {
                        Ok(rerooted) => {
                            self.data_dir = rerooted.data_dir;
                            self.store = rerooted.store;
                            self.outbox = rerooted.outbox;
                        }
                        Err(e) => {
                            return rpc::error_line(
                                Some(id),
                                rpc::INVALID_PARAMS,
                                &format!("dataDir unusable: {e}"),
                                None,
                            )
                        }
                    }
                }
            }
            if let Some(dev) = obj.get("devMode").and_then(Value::as_bool) {
                self.dev_mode = self.dev_mode || dev;
            }
            // Host feature negotiation (audit INT-003): `eventAck` = the host
            // durably journals every event.emit and confirms with an
            // `event.ack` notification. Outboxed events then stay pending
            // until acknowledged, and a replay thread re-delivers anything
            // unacknowledged — crash-safe, at-least-once, deduped by the
            // host on idempotencyKey. Hosts without the feature keep the
            // legacy write-marks-sent behaviour.
            let ack = obj
                .get("features")
                .and_then(Value::as_array)
                .is_some_and(|f| f.iter().any(|v| v.as_str() == Some("eventAck")));
            if ack && !self.ack_mode {
                self.ack_mode = true;
                // The replay thread writes real protocol lines to stdout —
                // unit tests exercise `replay_once` directly instead.
                if !cfg!(test) {
                    self.replay_heartbeat = Some(crate::event_bridge::spawn_replay_thread(
                        self.data_dir.join(OUTBOX_FILE),
                    ));
                }
                eprintln!("[aokie-plugin] host supports eventAck — durable delivery on");
            }
        }
        self.initialized = true;
        // Arm the live radio at handshake time (real mode) so a call ringing
        // before any command still reaches the flow. Non-blocking: init
        // status surfaces asynchronously via aokie.dongle.ready / hardware.error.
        self.ensure_radio_started();
        rpc::success_line(id, json!({"ok": true}))
    }

    fn handle_connector_request(
        &mut self,
        id: &Value,
        params: &Value,
        sink: &mut dyn Sink,
    ) -> String {
        // Validate the connector-request.schema.json shape defensively
        // even though Desktop validates first.
        let obj = match params.as_object() {
            Some(o) => o,
            None => {
                return rpc::error_line(
                    Some(id),
                    rpc::INVALID_PARAMS,
                    "connector.request params must be an object",
                    None,
                )
            }
        };
        for key in obj.keys() {
            if !matches!(
                key.as_str(),
                "connectorId" | "command" | "payload" | "timeoutMs" | "requestId"
            ) {
                return rpc::error_line(
                    Some(id),
                    rpc::INVALID_PARAMS,
                    &format!("unknown connector.request field: {key}"),
                    None,
                );
            }
        }
        let connector_id = obj.get("connectorId").and_then(Value::as_str).unwrap_or("");
        let command = obj.get("command").and_then(Value::as_str).unwrap_or("");
        if command.is_empty() {
            return rpc::error_line(
                Some(id),
                rpc::INVALID_PARAMS,
                "connector.request requires a string command",
                None,
            );
        }
        let request_id = obj.get("requestId").and_then(Value::as_str);
        if connector_id != CONNECTOR_ID {
            let err = CmdError {
                code: "connector_missing",
                message: format!(
                    "unknown connector: {connector_id:?} (this plugin serves \"aokie\")"
                ),
            };
            return connector_error_line(id, &err);
        }
        let payload = obj.get("payload").cloned().unwrap_or(Value::Null);
        match self.dispatch_command(command, &payload, sink) {
            Ok(data) => {
                let mut body = json!({"ok": true, "data": data});
                if let Some(rid) = request_id {
                    body["requestId"] = json!(rid);
                }
                rpc::success_line(id, body)
            }
            Err(err) => connector_error_line(id, &err),
        }
    }

    /// The MVP command surface. Every handler validates its payload
    /// (unknown fields rejected) before acting.
    pub fn dispatch_command(
        &mut self,
        command: &str,
        payload: &Value,
        sink: &mut dyn Sink,
    ) -> Result<Value, CmdError> {
        match command {
            "dongle.list" => {
                expect_fields(payload, &[])?;
                // The compatibility catalog: dongles Aokie is known to support (back-compat `dongles`).
                let dongles: Vec<Value> = dongle_catalog::list_known_dongles()
                    .into_iter()
                    .map(|d| {
                        let mut v = serde_json::to_value(d).expect("DongleId serialises");
                        v["source"] = json!("catalog");
                        v
                    })
                    .collect();
                // Live: actually plugged-in dongles, each flagged matchesCatalog (supported) and
                // driverBound (WinUSB attached = ready for Aokie). Lets the UI show "your dongle is
                // plugged in — install its driver" vs "ready".
                let (connected, live_err) = self.list_connected_dongles();
                Ok(json!({
                    "dongles": dongles,
                    "connected": connected,
                    "liveEnumeration": live_err.is_none(),
                    "note": live_err.unwrap_or_else(|| "connected[] are live USB devices; driverBound=true means the WinUSB driver is attached and the dongle is ready to pair.".to_string()),
                }))
            }
            "dongle.getPreferred" => {
                expect_fields(payload, &[])?;
                Ok(json!({"preferred": self.store.config.preferred_dongle}))
            }
            "dongle.setPreferred" => {
                let obj = expect_fields(payload, &["vid", "pid"])?;
                let vid = require_u16(&obj, "vid")?;
                let pid = require_u16(&obj, "pid")?;
                self.store.config.preferred_dongle = Some(PreferredDongle { vid, pid });
                self.save_config()?;
                Ok(json!({"preferred": {"vid": vid, "pid": pid}}))
            }
            "dongle.installDriver" => self.install_driver(payload),
            "dongle.restoreDriver" => self.restore_driver(payload),
            "dongle.removeCerts" => self.remove_certs(payload),
            "consent.get" => self.consent_get(),
            "consent.set" => self.consent_set(payload),
            "consent.revoke" => self.consent_revoke(),
            "dongle.diagnostics" => {
                let obj = expect_fields(payload, &["simulate"])?;
                match obj.get("simulate").and_then(Value::as_str) {
                    Some("call") if self.dev_mode => self.run_simulated_call(sink),
                    Some("call") => Err(CmdError::failed(
                        "dongle.diagnostics {simulate:\"call\"} requires dev mode (FORMLOGIC_DEV_MODE=1)",
                    )),
                    Some(other) => Err(CmdError::failed(format!(
                        "unknown simulate mode {other:?} (supported: \"call\")"
                    ))),
                    None => {
                        let c = self.outbox_counts()?;
                        if let Some(radio) = self.radio.as_ref() {
                            return Ok(json!({
                                "radio": {
                                    "initialized": radio.is_initialized(),
                                    "connected": radio.is_connected(),
                                    "callActive": radio.is_call_active(),
                                    "localAddress": radio.local_address(),
                                    "connectedPhone": radio.connected_address(),
                                    "deviceName": "Aokie AI Assistant",
                                    "error": radio.last_error(),
                                    // Speech results dropped because their call was
                                    // already over (audit C-05) — non-zero is fine,
                                    // growth per call is worth investigating.
                                    "staleSttResults": radio.stale_stt_results(),
                                    // AOK-VOICE-001: known voice-pipeline failures
                                    // (None = no known failure; set = auto-answer
                                    // is blocked + health degraded).
                                    "voiceSttError": radio.stt_error(),
                                    "voiceTtsError": radio.tts_error(),
                                },
                                // keyCollisions non-zero = a key-derivation bug
                                // rejected an event (audit AOK-EVENT-001).
                                "outbox": {
                                    "pending": c.pending,
                                    "failed": c.failed,
                                    "dead": c.dead,
                                    "keyCollisions": self.outbox.collision_count().unwrap_or(0),
                                },
                            }));
                        }
                        Err(CmdError::failed(format!(
                            "dongle.diagnostics is not yet wired to hardware (outbox: {} pending, {} failed, {} dead)",
                            c.pending, c.failed, c.dead
                        )))
                    }
                }
            }
            "phone.status" => {
                expect_fields(payload, &[])?;
                if let Some(radio) = self.radio.as_ref() {
                    let addr = radio.connected_address();
                    // AOK-BT-001: surface the bounded pairing window so the UI shows
                    // "discoverable, 95s left" vs the at-rest connectable-only state.
                    let pairing_secs = radio.pairing_window_remaining_secs();
                    // PAIR-001: the held SSP numeric comparison, if the radio is
                    // waiting on the operator (answered via phone.confirmPairing).
                    let pairing_confirm = radio.pending_pairing_confirm().map(|p| {
                        json!({
                            "address": p.address,
                            "numericValue": p.numeric_value,
                            "expiresEpochSecs": p.expires_epoch_secs,
                        })
                    });
                    return Ok(json!({
                        "paired": addr.is_some(),
                        "device": addr.as_ref().map(|a| json!({"address": a, "name": "Paired phone"})),
                        "connected": radio.is_connected(),
                        "initialized": radio.is_initialized(),
                        "callActive": radio.is_call_active(),
                        "localAddress": radio.local_address(),
                        "caller": radio.current_caller(),
                        "error": radio.last_error(),
                        // AOK-BT-001: discoverable ONLY inside an open pairing window.
                        "pairingOpen": pairing_secs > 0,
                        "pairingSecondsRemaining": pairing_secs,
                        "discoverable": pairing_secs > 0,
                        "pairingConfirm": pairing_confirm,
                        "source": "radio",
                    }));
                }
                let device = self.store.config.paired_devices.first();
                Ok(json!({
                    "paired": device.is_some(),
                    "device": device,
                    "pairingOpen": false,
                    "source": "config",
                }))
            }
            "phone.startPairing" => {
                // Optional {seconds} — how long to stay discoverable (default 120,
                // clamped 30..=300 so a stray value can't leave us open forever).
                let obj = expect_fields(payload, &["seconds"])?;
                let seconds = obj
                    .get("seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(120)
                    .clamp(30, 300);
                // AOK-CONSENT-001: pairing needs the bluetooth scope. Gives a
                // clear "consent required" error under enforce instead of the
                // generic radio-not-running one (the radio also won't start).
                self.check_consent("phone.startPairing", crate::consent::Scope::Bluetooth)?;
                // Ensure the radio is up, then open the bounded pairing window
                // (AOK-BT-001): the radio is connectable-only at rest, so this is
                // the ONLY way an unknown phone can discover + pair with us.
                self.ensure_radio_started();
                // Real mode with no radio: startup FAILED — a fabricated "pairing"
                // session would leave the operator waiting on a phone that can never
                // see us (FL-CONN-001). Dev mode returns the simulated stub.
                self.require_radio_or_dev("phone.startPairing")?;
                let Some(radio) = self.radio.as_ref() else {
                    let session_id = format!("pair_{}", uuid::Uuid::new_v4().simple());
                    let ev = aokie_event(
                        crate::contract::events::PHONE_PAIRING_STARTED,
                        &session_id,
                        json!({"at": now_iso8601(), "simulated": true}),
                    );
                    emit_event(sink, &self.outbox, &ev, false, crate::event_bridge::EmitMode::for_host(self.ack_mode, self.dev_mode || crate::event_bridge::legacy_host_allowed())).map_err(CmdError::failed)?;
                    return Ok(json!({"sessionId": session_id, "status": "pairing", "simulated": true}));
                };
                radio.start_pairing(seconds).map_err(CmdError::failed)?;
                let session_id = format!("pair_{}", uuid::Uuid::new_v4().simple());
                let ev = aokie_event(
                    crate::contract::events::PHONE_PAIRING_STARTED,
                    &session_id,
                    json!({"at": now_iso8601(), "windowSeconds": seconds}),
                );
                emit_event(sink, &self.outbox, &ev, false, crate::event_bridge::EmitMode::for_host(self.ack_mode, self.dev_mode || crate::event_bridge::legacy_host_allowed())).map_err(CmdError::failed)?;
                Ok(json!({
                    "sessionId": session_id,
                    "status": if radio.is_initialized() { "discoverable" } else { "starting" },
                    "deviceName": "Aokie AI Assistant",
                    "windowSeconds": seconds,
                    "initialized": radio.is_initialized(),
                    "localAddress": radio.local_address(),
                    "error": radio.last_error(),
                    "note": "Open your phone's Bluetooth and pair with \"Aokie AI Assistant\" within the window.",
                }))
            }
            "phone.stopPairing" => {
                expect_fields(payload, &["sessionId"])?;
                if self.dev_mode {
                    return Ok(json!({"stopped": true, "simulated": true}));
                }
                // AOK-BT-001: close the discoverable window now (back to
                // connectable-only). Idempotent — closing an already-closed window
                // is fine and still reports stopped.
                self.require_radio_or_dev("phone.stopPairing")?;
                if let Some(radio) = self.radio.as_ref() {
                    radio.stop_pairing().map_err(CmdError::failed)?;
                }
                Ok(json!({"stopped": true}))
            }
            "phone.listPaired" => {
                expect_fields(payload, &[])?;
                if let Some(radio) = self.radio.as_ref() {
                    // AOK-BT-001: the BONDED devices in the pairing store are the
                    // revocable identities (removePaired targets these) — not the
                    // live-session connection view.
                    let devices: Vec<Value> = radio
                        .list_bonded()
                        .map_err(CmdError::failed)?
                        .into_iter()
                        .map(|address| json!({"address": address}))
                        .collect();
                    return Ok(json!({"devices": devices}));
                }
                Ok(json!({"devices": self.store.config.paired_devices}))
            }
            "phone.removePaired" => {
                // AOK-BT-001: forget a bonded device so it can no longer reconnect
                // without pairing again. PAIR-001: an active session for that
                // device is disconnected first.
                let obj = expect_fields(payload, &["address"])?;
                let address = require_str(&obj, "address")?;
                self.require_radio_or_dev("phone.removePaired")?;
                if let Some(radio) = self.radio.as_ref() {
                    let removed = radio.remove_paired(address.clone()).map_err(CmdError::failed)?;
                    return Ok(json!({"removed": removed, "address": address}));
                }
                // Dev mode: nothing bonded to remove.
                Ok(json!({"removed": false, "address": address, "simulated": true}))
            }
            "phone.confirmPairing" => {
                // PAIR-001: the operator's answer to the SSP numeric comparison
                // (phone.status.pairingConfirm / the pairing_confirm_required
                // event). Same consent scope as starting the pairing window.
                let obj = expect_fields(payload, &["address", "accept"])?;
                let address = require_str(&obj, "address")?;
                let accept = obj
                    .get("accept")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| CmdError::failed("confirmPairing needs a boolean `accept`"))?;
                self.check_consent("phone.confirmPairing", crate::consent::Scope::Bluetooth)?;
                self.require_radio_or_dev("phone.confirmPairing")?;
                if let Some(radio) = self.radio.as_ref() {
                    radio
                        .confirm_pairing(address.clone(), accept)
                        .map_err(CmdError::failed)?;
                    return Ok(json!({"address": address, "accepted": accept}));
                }
                // Dev mode: nothing is ever pending.
                Err(CmdError::failed(
                    "no pairing confirmation is pending (simulated)",
                ))
            }
            "call.current" => {
                expect_fields(payload, &[])?;
                if let Some(radio) = self.radio.as_ref() {
                    // Canonical shape (audit C-02) — the SAME keys the browser
                    // mock returns and the Live Call screen parses, so a
                    // refreshed page recovers a real in-flight call.
                    let call = radio.current_call_id().map(|call_id| {
                        json!({
                            "callId": call_id,
                            "from": radio.current_caller(),
                            "state": if radio.is_call_active() {
                                crate::contract::call_state::ACTIVE
                            } else {
                                crate::contract::call_state::RINGING
                            },
                            "startedAt": radio.call_started_at(),
                        })
                    });
                    return Ok(json!({"call": call}));
                }
                self.require_radio_or_dev("call.current")?;
                Ok(json!({"call": self.mock.current_call.as_ref().map(call_json)}))
            }
            "call.answer" => {
                let obj = expect_fields(payload, &["callId"])?;
                let call_id = optional_str(&obj, "callId")?;
                if let Some(radio) = self.radio.as_ref() {
                    check_call_id(call_id.as_deref(), radio.current_call_id().as_deref())?;
                    // AOK-CTRL-001: the result reports ACCEPTANCE, never the
                    // final verb — the phone hasn't acted yet. The authoritative
                    // confirmation is the `aokie.call.answered` event (or a
                    // `hardware.error` code `control_failed` carrying this
                    // operationId if the radio action fails).
                    let op = operation_id();
                    radio
                        .send(crate::radio::RadioControl::Answer {
                            op: Some(op.clone()),
                        })
                        .map_err(CmdError::failed)?;
                    return Ok(json!({
                        "accepted": true,
                        "queued": true,
                        "operationId": op,
                        "via": "radio",
                        "callId": radio.current_call_id(),
                    }));
                }
                self.require_radio_or_dev("call.answer")?;
                check_call_id(call_id.as_deref(), self.mock_call_id().as_deref())?;
                let call = self.require_call(&[MockCallState::Incoming], "call.answer")?;
                call.state = MockCallState::Active;
                let (corr, snapshot) = {
                    let c = self.mock.current_call.as_ref().unwrap();
                    (c.correlation_id.clone(), call_json(c))
                };
                let ev = aokie_event(crate::contract::events::CALL_ANSWERED, &corr, json!({"at": now_iso8601()}));
                emit_event(sink, &self.outbox, &ev, false, crate::event_bridge::EmitMode::for_host(self.ack_mode, self.dev_mode || crate::event_bridge::legacy_host_allowed())).map_err(CmdError::failed)?;
                Ok(json!({"accepted": true, "answered": true, "call": snapshot}))
            }
            "call.reject" => {
                let obj = expect_fields(payload, &["callId"])?;
                let call_id = optional_str(&obj, "callId")?;
                if let Some(radio) = self.radio.as_ref() {
                    check_call_id(call_id.as_deref(), radio.current_call_id().as_deref())?;
                    // AOK-CTRL-001: acceptance only — `aokie.call.ended`
                    // (outcome "rejected") is the authoritative confirmation.
                    let op = operation_id();
                    radio
                        .send(crate::radio::RadioControl::Reject {
                            op: Some(op.clone()),
                        })
                        .map_err(CmdError::failed)?;
                    return Ok(json!({
                        "accepted": true,
                        "queued": true,
                        "operationId": op,
                        "via": "radio",
                        "callId": radio.current_call_id(),
                    }));
                }
                self.require_radio_or_dev("call.reject")?;
                check_call_id(call_id.as_deref(), self.mock_call_id().as_deref())?;
                let call = self.require_call(&[MockCallState::Incoming], "call.reject")?;
                call.state = MockCallState::Ended;
                let corr = call.correlation_id.clone();
                let ev = aokie_event(crate::contract::events::CALL_REJECTED, &corr, json!({"at": now_iso8601()}));
                emit_event(sink, &self.outbox, &ev, false, crate::event_bridge::EmitMode::for_host(self.ack_mode, self.dev_mode || crate::event_bridge::legacy_host_allowed())).map_err(CmdError::failed)?;
                Ok(json!({"accepted": true, "rejected": true}))
            }
            "call.hangup" => {
                let obj = expect_fields(payload, &["callId"])?;
                let call_id = optional_str(&obj, "callId")?;
                if let Some(radio) = self.radio.as_ref() {
                    check_call_id(call_id.as_deref(), radio.current_call_id().as_deref())?;
                    // AOK-CTRL-001: acceptance only — `aokie.call.ended` is the
                    // authoritative confirmation the call actually ended.
                    let op = operation_id();
                    radio
                        .send(crate::radio::RadioControl::Hangup {
                            op: Some(op.clone()),
                        })
                        .map_err(CmdError::failed)?;
                    return Ok(json!({
                        "accepted": true,
                        "queued": true,
                        "operationId": op,
                        "via": "radio",
                        "callId": radio.current_call_id(),
                    }));
                }
                self.require_radio_or_dev("call.hangup")?;
                check_call_id(call_id.as_deref(), self.mock_call_id().as_deref())?;
                let call = self.require_call(
                    &[MockCallState::Incoming, MockCallState::Active],
                    "call.hangup",
                )?;
                call.state = MockCallState::Ended;
                let (corr, turns) = (call.correlation_id.clone(), call.turns);
                let call_from = call.caller.clone();
                let ev = aokie_event(
                    crate::contract::events::CALL_ENDED,
                    &corr,
                    json!({
                        "at": now_iso8601(),
                        "turns": turns,
                        "reason": "operator_hangup",
                        "callId": corr,
                        "from": call_from,
                        "callerPhone": call_from,
                        "durationSeconds": 30,
                        "outcome": "completed",
                    }),
                );
                emit_event(sink, &self.outbox, &ev, false, crate::event_bridge::EmitMode::for_host(self.ack_mode, self.dev_mode || crate::event_bridge::legacy_host_allowed())).map_err(CmdError::failed)?;
                Ok(json!({"accepted": true, "ended": true}))
            }
            "call.operatorSpeak" => {
                let obj = expect_fields(payload, &["text", "callId"])?;
                let text = require_str(&obj, "text")?;
                if text.trim().is_empty() {
                    return Err(CmdError::failed("text is empty"));
                }
                let call_id = optional_str(&obj, "callId")?;
                if let Some(radio) = self.radio.as_ref() {
                    // Truthfulness gate (audit INT-006/C-15): a build without
                    // voice output can only LOG the text — never claim it was
                    // spoken to the caller.
                    if !cfg!(feature = "voice") {
                        return Err(CmdError::failed(
                            "this plugin build has no voice output (voice feature not compiled) — operatorSpeak cannot be spoken",
                        ));
                    }
                    // AOK-CTRL-001: while the RUNNING radio's in-plugin agent
                    // owns replies, the radio drops operatorSpeak (the caller
                    // must not be answered twice) — so accepting it here would
                    // be a lie. Refuse typed instead.
                    if radio
                        .status
                        .agent_enabled
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        return Err(CmdError::failed(
                            "the in-plugin AI receptionist owns replies on this install — operatorSpeak would talk over it and is refused (disable the aiReceptionist setting to speak manually)",
                        ));
                    }
                    check_call_id(call_id.as_deref(), radio.current_call_id().as_deref())?;
                    // Acceptance only: the bot `call.turn.final` event confirms
                    // the text actually played; a silent synthesis emits
                    // `hardware.error` code `speak_failed` with this id.
                    let op = operation_id();
                    radio
                        .send(crate::radio::RadioControl::Speak {
                            text: text.clone(),
                            op: Some(op.clone()),
                        })
                        .map_err(CmdError::failed)?;
                    return Ok(json!({
                        "accepted": true,
                        "queued": true,
                        "operationId": op,
                        "via": "radio",
                    }));
                }
                self.require_radio_or_dev("call.operatorSpeak")?;
                check_call_id(call_id.as_deref(), self.mock_call_id().as_deref())?;
                let call = self.require_call(&[MockCallState::Active], "call.operatorSpeak")?;
                call.turns += 1;
                // Mock: no audio path yet — acknowledge without faking
                // a TTS round-trip result.
                Ok(json!({"accepted": true, "spoken": true, "mock": true}))
            }
            "sms.threads" => {
                expect_fields(payload, &[])?;
                // FL-CONN-001: this reads the dev simulator's in-memory threads —
                // it has no real-radio backing yet (AOK-SMS-001), so in real mode
                // it must fail typed instead of presenting fake history.
                self.require_radio_or_dev("sms.threads")?;
                if !self.dev_mode {
                    return Err(CmdError::failed(
                        "sms.threads: real thread history is not available from the radio yet — sent/received messages are recorded in FormLogic",
                    ));
                }
                let threads: Vec<Value> = self
                    .mock
                    .sms_threads
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "phone": t.phone,
                            "messageCount": t.messages.len(),
                            "lastMessageAt": t.messages.last().map(|m| m.at.clone()),
                        })
                    })
                    .collect();
                Ok(json!({"threads": threads}))
            }
            "sms.thread" => {
                let obj = expect_fields(payload, &["threadId"])?;
                let thread_id = require_str(&obj, "threadId")?;
                self.require_radio_or_dev("sms.thread")?;
                if !self.dev_mode {
                    return Err(CmdError::failed(
                        "sms.thread: real thread history is not available from the radio yet — sent/received messages are recorded in FormLogic",
                    ));
                }
                let thread = self
                    .mock
                    .sms_threads
                    .iter()
                    .find(|t| t.id == thread_id)
                    .ok_or_else(|| CmdError::failed(format!("unknown thread: {thread_id}")))?;
                let messages: Vec<Value> = thread
                    .messages
                    .iter()
                    .map(|m| {
                        json!({"id": m.id, "direction": m.direction, "body": m.body, "at": m.at})
                    })
                    .collect();
                Ok(json!({"id": thread.id, "phone": thread.phone, "messages": messages}))
            }
            "sms.send" => {
                let obj = expect_fields(payload, &["to", "body"])?;
                let to = require_str(&obj, "to")?;
                let body = require_str(&obj, "body")?;
                let to = validate_sms_recipient(&to).map_err(CmdError::failed)?;
                let body = validate_sms_body(&body).map_err(CmdError::failed)?;
                // AOK-CONSENT-001: sending SMS (MAP) needs the `sms` scope.
                self.check_consent("sms.send", crate::consent::Scope::Sms)?;
                if let Some(radio) = self.radio.as_ref() {
                    // The runtime builds the bMessage + PushMessage; the
                    // aokie.sms.sent event (with its handle) is emitted by the
                    // radio thread when the AG acks the PUT.
                    radio
                        .send(crate::radio::RadioControl::SendSms {
                            to: to.clone(),
                            body,
                        })
                        .map_err(CmdError::failed)?;
                    return Ok(json!({"to": to, "status": "queued", "via": "radio"}));
                }
                // Real mode with no radio: the message can NOT be sent — a
                // fabricated "queued" here is the audit's canonical fake
                // success (a caller was promised an SMS that never existed).
                self.require_radio_or_dev("sms.send")?;
                let message_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
                let at = now_iso8601();
                // Essential event: outboxed before emission. The
                // correlation id is the SMS handle (contract §3).
                let ev = aokie_event(
                    crate::contract::events::SMS_SENT,
                    &message_id,
                    json!({"messageId": message_id, "to": to, "at": at, "simulated": true}),
                );
                emit_event(sink, &self.outbox, &ev, false, crate::event_bridge::EmitMode::for_host(self.ack_mode, self.dev_mode || crate::event_bridge::legacy_host_allowed())).map_err(CmdError::failed)?;
                let thread = self.mock.thread_for(&to);
                thread.messages.push(MockSmsMessage {
                    id: message_id.clone(),
                    direction: "out",
                    body,
                    at,
                });
                Ok(json!({"messageId": message_id, "status": "queued", "simulated": true}))
            }
            "outbox.redrive" => {
                // Operator redrive (audit OBS-001 / AOK-OUTBOX-001): TARGETED
                // by default — one idempotencyKey revives one row; replaying
                // the whole dead set requires an explicit {"all": true} so a
                // casual redrive can never resurrect an entire historical
                // queue by accident.
                let key = payload.get("idempotencyKey").and_then(Value::as_str);
                let all = payload.get("all").and_then(Value::as_bool) == Some(true);
                if key.is_none() && !all {
                    return Err(CmdError::failed(
                        "outbox.redrive needs {idempotencyKey: \"…\"} for one event, or an explicit {all: true} for the whole dead set",
                    ));
                }
                let revived = self
                    .outbox
                    .redrive_dead(key)
                    .map_err(|e| CmdError::failed(format!("outbox redrive failed: {e}")))?;
                if revived > 0 {
                    eprintln!("[aokie-plugin] operator redrive revived {revived} dead outbox event(s)");
                }
                Ok(json!({ "revived": revived }))
            }
            "settings.get" => {
                let obj = expect_fields(payload, &["key"])?;
                match obj.get("key").and_then(Value::as_str) {
                    Some(key) => Ok(json!({
                        "key": key,
                        "value": self.store.config.settings.get(key).cloned(),
                    })),
                    None => Ok(json!({
                        "settings": self.store.config.settings,
                        "configVersion": self.store.config.config_version,
                        "configQuarantined": self.store.quarantined,
                    })),
                }
            }
            "settings.set" => {
                let obj = payload.as_object().ok_or_else(|| {
                    CmdError::failed("settings.set payload must be an object of key/value pairs")
                })?;
                if obj.is_empty() {
                    return Err(CmdError::failed("settings.set payload is empty"));
                }
                // Typed validation (audit AK-006) — covers the endpoint-URL
                // classification (PRIV-001/C-16) for aiEndpoint/sttEndpoint/
                // ttsEndpoint. ALL keys validate before ANY persist: a batch
                // with one bad key changes nothing.
                for (key, value) in obj {
                    validate_setting(key, value)?;
                }
                // CONSENT-001 destination enforcement: under `enforce` with a
                // recorded grant, a NON-LOOPBACK ai/stt/tts endpoint must be
                // one the operator consented to (grant.scopes.destinations) —
                // pointing transcripts at a new remote processor is a material
                // change that requires re-consent, not a silent settings edit.
                if self.consent_mode() == crate::consent::ConsentMode::Enforce {
                    if let Some(grant) = self.consent_loaded().grant.as_ref() {
                        for key in ["aiEndpoint", "sttEndpoint", "ttsEndpoint"] {
                            let Some(url) = obj.get(key).and_then(Value::as_str) else { continue };
                            let url = url.trim();
                            if url.is_empty() || is_loopback_endpoint(url) {
                                continue; // clearing / local processing needs no destination grant
                            }
                            if !grant.scopes.destinations.iter().any(|d| d.trim() == url) {
                                return Err(CmdError::failed(format!(
                                    "{key}: {url} is not a consented destination — re-run the \
                                     FormLogic consent wizard to add it before use"
                                )));
                            }
                        }
                    }
                }
                for (key, value) in obj {
                    self.store
                        .config
                        .settings
                        .insert(key.clone(), value.clone());
                }
                self.store.config.config_version += 1;
                self.save_config()?;
                // Stamp the live revision into the radio status so the NEXT
                // call.ended records which configuration it ran under
                // (audit AOK-CONFIG-002).
                if let Some(radio) = self.radio.as_ref() {
                    radio
                        .status
                        .config_version
                        .store(self.store.config.config_version, std::sync::atomic::Ordering::Relaxed);
                }
                // Live-reconfigure a running receptionist so a flow (or the desktop)
                // can push the Receptionist Settings — persona/greeting/voice/model —
                // and have them take effect on the current call, no reconnect. Only
                // the agent-shaping keys trip this; other settings just persist.
                let agent_key = has_receptionist_config_key(obj);
                let radio_running = self.radio.is_some();
                if agent_key {
                    if let Some(radio) = self.radio.as_ref() {
                        let _ = radio.send(crate::radio::RadioControl::Configure {
                            persona: string_setting(obj, "persona"),
                            greeting: string_setting(obj, "greeting"),
                            voice: string_setting(obj, "ttsVoice"),
                            model: string_setting(obj, "aiModel"),
                            endpoint: string_setting(obj, "aiEndpoint"),
                            stt_endpoint: string_setting(obj, "sttEndpoint"),
                            tts_endpoint: string_setting(obj, "ttsEndpoint"),
                        });
                    }
                }
                // Truthful apply-state per key (audit AK-006): live keys only
                // count as applied when a radio is actually running to receive
                // the Configure; everything else waits for the next connect.
                let mut applied_live: Vec<&String> = Vec::new();
                let mut applies_at_reconnect: Vec<&String> = Vec::new();
                for key in obj.keys() {
                    match setting_spec(key) {
                        Some(spec) if spec.applies_live && radio_running => applied_live.push(key),
                        Some(_) => applies_at_reconnect.push(key),
                        None => {}
                    }
                }
                Ok(json!({
                    "settings": self.store.config.settings,
                    "configVersion": self.store.config.config_version,
                    "appliedLive": applied_live,
                    "appliesAtReconnect": applies_at_reconnect,
                }))
            }
            other => Err(CmdError::failed(format!("unknown command: {other}"))),
        }
    }

    /// Dev-mode scripted lifecycle (contract §4): one correlation id,
    /// contract-ordered events, every step written to the outbox.
    fn run_simulated_call(&mut self, sink: &mut dyn Sink) -> Result<Value, CmdError> {
        let corr = format!("call_{}", uuid::Uuid::new_v4().simple());
        let caller = "+61412345678";
        let dongle = dongle_catalog::DEFAULT_CATALOG[0];
        let started_at = now_iso8601();

        let mut emitted: Vec<String> = Vec::new();
        let mut emit = |plugin: &mut Plugin, ev: aokie_core::events::DesktopEvent| {
            // The scripted run records EVERY step in the outbox so
            // integration tests can assert write-before-emit.
            emit_event(sink, &plugin.outbox, &ev, true, crate::event_bridge::EmitMode::for_host(plugin.ack_mode, plugin.dev_mode || crate::event_bridge::legacy_host_allowed())).map_err(CmdError::failed)?;
            emitted.push(ev.name.clone());
            Ok::<(), CmdError>(())
        };

        emit(
            self,
            aokie_event(
                crate::contract::events::DONGLE_DETECTED,
                &corr,
                json!({"vid": dongle.vid, "pid": dongle.pid, "tier": dongle.tier, "source": "mock"}),
            ),
        )?;
        emit(
            self,
            aokie_event(
                crate::contract::events::DONGLE_READY,
                &corr,
                json!({"vid": dongle.vid, "pid": dongle.pid}),
            ),
        )?;
        emit(
            self,
            aokie_event(
                crate::contract::events::CALL_INCOMING,
                &corr,
                json!({"from": caller, "at": started_at}),
            ),
        )?;
        self.mock.current_call = Some(MockCall {
            correlation_id: corr.clone(),
            caller: caller.to_string(),
            state: MockCallState::Incoming,
            started_at: started_at.clone(),
            turns: 0,
        });

        emit(
            self,
            aokie_event(crate::contract::events::CALL_ANSWERED, &corr, json!({"at": now_iso8601()})),
        )?;
        if let Some(call) = self.mock.current_call.as_mut() {
            call.state = MockCallState::Active;
        }

        emit(
            self,
            aokie_turn_event(
                true,
                &corr,
                1,
                json!({"speaker": "caller", "text": "Hi, I'd like to book an appointment for tomorrow morning."}),
            ),
        )?;
        emit(
            self,
            aokie_turn_event(
                true,
                &corr,
                2,
                json!({"speaker": "bot", "text": "Sure — we have 9:30 am available. I'll text you a confirmation."}),
            ),
        )?;
        if let Some(call) = self.mock.current_call.as_mut() {
            call.turns = 2;
        }

        emit(
            self,
            aokie_event(
                crate::contract::events::CALL_ENDED,
                &corr,
                json!({
                    "at": now_iso8601(),
                    "durationMs": 42_000,
                    "turns": 2,
                    "callId": corr,
                    "from": caller,
                    "callerPhone": caller,
                    "durationSeconds": 42,
                    "outcome": "completed",
                }),
            ),
        )?;
        if let Some(call) = self.mock.current_call.as_mut() {
            call.state = MockCallState::Ended;
        }

        let sms_at = now_iso8601();
        let sms_body = "CONFIRM 9:30am";
        emit(
            self,
            aokie_event(
                crate::contract::events::SMS_RECEIVED,
                &corr,
                json!({"from": caller, "body": sms_body, "at": sms_at}),
            ),
        )?;
        let thread = self.mock.thread_for(caller);
        let msg_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
        thread.messages.push(MockSmsMessage {
            id: msg_id,
            direction: "in",
            body: sms_body.to_string(),
            at: sms_at,
        });

        let counts = self.outbox_counts()?;
        Ok(json!({
            "simulated": "call",
            "correlationId": corr,
            "events": emitted,
            "outbox": {
                "pending": counts.pending,
                "sent": counts.sent,
                "failed": counts.failed,
                "dead": counts.dead,
            },
        }))
    }

    /// Truthful health (audit INT-006/C-15): computed from what this build
    /// and process can actually DO, never a constant "ok". `degraded` means
    /// answering/speaking is impaired (no voice output compiled, radio not
    /// up in real mode, dead outbox rows awaiting redrive); components let
    /// the host show WHICH dependency broke.
    fn build_health(&self) -> Value {
        let voice = cfg!(feature = "voice");
        let counts = self.outbox.counts().unwrap_or_default();
        let mut reasons: Vec<String> = Vec::new();
        if !voice {
            reasons.push("voice feature not compiled — the receptionist cannot speak".to_string());
        }
        let radio = match self.radio.as_ref() {
            Some(r) => {
                if let Some(e) = r.last_error() {
                    reasons.push(format!("radio error: {e}"));
                } else if !r.is_initialized() {
                    reasons.push("radio starting (dongle not initialised yet)".to_string());
                } else if !r.is_connected() {
                    // A receptionist line with no phone linked CANNOT take
                    // calls — health must not read "ok" on a dead line
                    // (observed live 2026-07-13: a mid-call Bluetooth
                    // supervision timeout dropped the link and the phone
                    // never reconnected; health stayed green while every
                    // subsequent call rang unanswered). The fix is on the
                    // phone: reconnect to the dongle from Bluetooth settings.
                    reasons.push(
                        "no phone connected — the receptionist cannot take calls; reconnect the phone to 'Aokie AI Assistant' in its Bluetooth settings"
                            .to_string(),
                    );
                }
                // AOK-VOICE-001: a KNOWN voice failure (asset preflight / live
                // engine or synthesis failure) is a concrete degraded state —
                // auto-answer is blocked while either slot is set, so health
                // must say WHY the receptionist isn't picking up.
                let stt_err = r.stt_error();
                let tts_err = r.tts_error();
                if let Some(e) = &stt_err {
                    reasons.push(format!("voice: {e}"));
                }
                if let Some(e) = &tts_err {
                    reasons.push(format!("voice: {e}"));
                }
                // VOICE-001: the loopback self-test — running (None) or failed
                // both degrade readiness (auto-answer is blocked either way).
                let self_test = r.self_test();
                match &self_test {
                    None => reasons.push(
                        "voice self-test still running — auto-answer arms once it passes"
                            .to_string(),
                    ),
                    Some(st) if !st.ok => {
                        reasons.push(format!("voice self-test failed: {}", st.detail))
                    }
                    Some(_) => {}
                }
                json!({
                    "present": true,
                    "initialized": r.is_initialized(),
                    "phoneConnected": r.is_connected(),
                    "callActive": r.is_call_active(),
                    "staleSttResults": r.stale_stt_results(),
                    "error": r.last_error(),
                    "voiceRuntime": {
                        "ready": stt_err.is_none() && tts_err.is_none()
                            && self_test.as_ref().is_some_and(|st| st.ok),
                        "sttError": stt_err,
                        "ttsError": tts_err,
                        "selfTest": self_test.map(|st| json!({
                            "ok": st.ok,
                            "at": st.at,
                            "durationMs": st.duration_ms,
                            "detail": st.detail,
                        })),
                    },
                })
            }
            None => {
                if !self.dev_mode {
                    let cause = self
                        .radio_start_error
                        .as_deref()
                        .unwrap_or("no dongle / driver not bound");
                    reasons.push(format!("radio not running ({cause})"));
                }
                json!({ "present": false, "startError": self.radio_start_error })
            }
        };
        if counts.dead > 0 {
            reasons.push(format!("{} dead outbox event(s) need redrive", counts.dead));
        }
        // AOK-DUR-001 item 3: an ack-incapable PRODUCTION host is UNHEALTHY —
        // essential events are held in the outbox (RequireAck) rather than
        // degraded to write-means-sent; with the explicit override the
        // downgrade is still surfaced. Dev mode (mock host) is exempt.
        if self.initialized && !self.ack_mode && !self.dev_mode {
            reasons.push(if crate::event_bridge::legacy_host_allowed() {
                "host does not support eventAck — legacy write-means-sent delivery in use (explicitly allowed)"
                    .to_string()
            } else {
                "host does not support eventAck — essential events are HELD in the outbox until an ack-capable host connects"
                    .to_string()
            });
        }
        // Replay-thread liveness (audit AOK-OUTBOX-002): durable delivery
        // silently freezing is exactly the failure health must not hide.
        let replay_age = self.replay_heartbeat.as_ref().map(|hb| {
            let v = hb.load(std::sync::atomic::Ordering::Relaxed);
            if v == 0 { u64::MAX } else { crate::event_bridge::unix_now().saturating_sub(v) }
        });
        match replay_age {
            Some(u64::MAX) => reasons.push(
                "outbox replay thread failed to start — durable delivery halted".to_string(),
            ),
            Some(age) if age > 30 => reasons.push(format!(
                "outbox replay thread stalled ({age}s since last tick) — durable delivery frozen"
            )),
            _ => {}
        }
        if self.store.quarantined {
            reasons.push(
                "settings file was corrupt — quarantined; recovered last-known-good backup or safe defaults (auto-answer OFF)"
                    .to_string(),
            );
        }
        // AOK-CONSENT-001: consent posture. A hard block (enforce + no valid
        // grant) or an unrecorded-consent warning both degrade readiness so
        // the operator sees WHY the phone is offline / at risk.
        let consent_mode = self.consent_mode();
        let consent_loaded = self.consent_loaded();
        let consent_grant = consent_loaded.grant.clone();
        let consent_decision = crate::consent::evaluate(
            consent_grant.as_ref(),
            crate::consent::CURRENT_CONSENT_VERSION,
            consent_mode,
            crate::consent::Scope::Bluetooth,
            &aokie_core::events::now_iso8601(),
        );
        // Only a DENY (enforce mode, actually blocking the radio) degrades
        // readiness. In `warn` mode the operator can't act on it yet (no
        // wizard) and sensitive work still runs, so it's surfaced in the
        // consent component below — not flagged as degraded, which would be
        // permanent amber noise on every install.
        if let crate::consent::ConsentDecision::Deny(r) = &consent_decision {
            reasons.push(format!("consent required: {r}"));
        }
        let consent = json!({
            "mode": consent_mode.as_str(),
            "recorded": consent_grant.is_some(),
            "recordedVersion": consent_grant.as_ref().map(|g| g.version),
            "requiredVersion": crate::consent::CURRENT_CONSENT_VERSION,
            "bluetoothAllowed": !consent_decision.is_denied(),
            "blocked": self.consent_blocked,
        });
        // PROC-001 item 4: the RESPONDER — whatever produces the receptionist's
        // replies — is part of readiness, not just ears (STT) and mouth (TTS).
        //   agent mode (aiReceptionist on): the in-plugin agent needs a live LLM;
        //     the radio's background probe records reachability in llm_error and
        //     auto-answer is blocked while it is set.
        //   flow mode: replies come from HOST flows — the plugin can't probe the
        //     host's flow link from here, so health names where to look instead
        //     of guessing.
        let agent_mode = self
            .store
            .config
            .settings
            .get("aiReceptionist")
            .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
            .unwrap_or(false);
        let llm_err = self.radio.as_ref().and_then(|r| r.llm_error());
        if agent_mode {
            if let Some(e) = &llm_err {
                reasons.push(format!(
                    "responder: {e} — auto-answer is blocked (calls ring through) until the LLM recovers"
                ));
            }
        }
        let responder = json!({
            "mode": if agent_mode { "agent" } else { "flow" },
            // Flow mode reads `ready` (the plugin can't disprove it); the note
            // says where the real check lives.
            "ready": !agent_mode || llm_err.is_none(),
            "llmError": llm_err,
            "note": if agent_mode {
                Value::Null
            } else {
                json!("replies are produced by host flows — check the desktop's FormLogic link and the reply flow binding")
            },
        });
        json!({
            "status": if reasons.is_empty() { "ok" } else { "degraded" },
            "detail": if reasons.is_empty() { Value::Null } else { json!(reasons.join("; ")) },
            "components": {
                "voice": voice,
                "devMode": self.dev_mode,
                "radio": radio,
                "responder": responder,
                "consent": consent,
                "outbox": {
                    "pending": counts.pending,
                    "failed": counts.failed,
                    "dead": counts.dead,
                    // AOK-DUR-001: whether the host confirms durable receipt.
                    "ackMode": self.ack_mode,
                },
                "config": {
                    "version": self.store.config.config_version,
                    "quarantined": self.store.quarantined,
                },
                // Build provenance (audit CROSS-OBS-001): WHICH build answered.
                "build": {
                    "version": env!("CARGO_PKG_VERSION"),
                    "ref": env!("AOKIE_BUILD_REF"),
                },
            },
        })
    }

    /// The mock's current call id (any state) — the `callId` guard target
    /// when no radio is present.
    fn mock_call_id(&self) -> Option<String> {
        self.mock
            .current_call
            .as_ref()
            .map(|c| c.correlation_id.clone())
    }

    fn require_call(
        &mut self,
        allowed: &[MockCallState],
        command: &str,
    ) -> Result<&mut MockCall, CmdError> {
        match self.mock.current_call.as_mut() {
            Some(call) if allowed.contains(&call.state) => Ok(call),
            Some(call) => Err(CmdError::failed(format!(
                "{command}: call {} is {}, not {}",
                call.correlation_id,
                call.state.as_str(),
                allowed
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("/")
            ))),
            None => Err(CmdError::failed(format!("{command}: no active call"))),
        }
    }

    fn save_config(&self) -> Result<(), CmdError> {
        self.store
            .save()
            .map_err(|e| CmdError::failed(format!("cannot persist settings: {e}")))
    }

    /// Bind the WinUSB driver to a Bluetooth dongle so the aokie radio stack can claim it. vid/pid
    /// come from the payload (`{vid, pid}`), else the configured preferred dongle. Windows-only
    /// (WinUSB); the elevated `aokie-driver-helper.exe` performs the install, so a UAC prompt appears
    /// on the machine running FormLogic Desktop. Missing helper / cancelled elevation / absent dongle
    /// surface as a typed command_failed the web caller sees — no panic.
    #[cfg(target_os = "windows")]
    fn install_driver(&self, payload: &Value) -> Result<Value, CmdError> {
        let obj = payload.as_object().cloned().unwrap_or_default();
        let (vid, pid) = if obj.contains_key("vid") || obj.contains_key("pid") {
            (require_u16(&obj, "vid")?, require_u16(&obj, "pid")?)
        } else if let Some(pref) = &self.store.config.preferred_dongle {
            (pref.vid, pref.pid)
        } else {
            return Err(CmdError::failed(
                "dongle.installDriver needs vid + pid (or set a preferred dongle via dongle.setPreferred first)",
            ));
        };
        let work_dir = self.data_dir.join("winusb-install");
        match aokie_dongle::installer::install_winusb(vid, pid, &work_dir) {
            Ok(_) => Ok(json!({
                "installed": true,
                "vid": vid,
                "pid": pid,
                "backend": "aokie-helper",
            })),
            Err(e) => Err(CmdError::failed(format!(
                "WinUSB driver install failed: {e}"
            ))),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn install_driver(&self, _payload: &Value) -> Result<Value, CmdError> {
        Err(CmdError::failed(
            "dongle.installDriver is only supported on Windows (WinUSB)",
        ))
    }

    /// AOK-DRIVER-001: revert a dongle Aokie bound to WinUSB back to its
    /// in-box driver. Elevated (UAC on the host). vid/pid from the payload
    /// or the configured preferred dongle.
    #[cfg(target_os = "windows")]
    fn restore_driver(&self, payload: &Value) -> Result<Value, CmdError> {
        let obj = payload.as_object().cloned().unwrap_or_default();
        let (vid, pid) = if obj.contains_key("vid") || obj.contains_key("pid") {
            (require_u16(&obj, "vid")?, require_u16(&obj, "pid")?)
        } else if let Some(pref) = &self.store.config.preferred_dongle {
            (pref.vid, pref.pid)
        } else {
            return Err(CmdError::failed(
                "dongle.restoreDriver needs vid + pid (or set a preferred dongle via dongle.setPreferred first)",
            ));
        };
        let work_dir = self.data_dir.join("winusb-install");
        match aokie_dongle::installer::restore_original_driver(vid, pid, &work_dir) {
            Ok(()) => Ok(json!({"restored": true, "vid": vid, "pid": pid})),
            Err(e) => Err(CmdError::failed(format!("restore original driver failed: {e}"))),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn restore_driver(&self, _payload: &Value) -> Result<Value, CmdError> {
        Err(CmdError::failed(
            "dongle.restoreDriver is only supported on Windows (WinUSB)",
        ))
    }

    /// AOK-DRIVER-001: remove every Aokie-signed certificate from the
    /// machine trust stores (clean-uninstall path). Elevated (UAC).
    #[cfg(target_os = "windows")]
    fn remove_certs(&self, payload: &Value) -> Result<Value, CmdError> {
        expect_fields(payload, &[])?;
        let work_dir = self.data_dir.join("winusb-install");
        match aokie_dongle::installer::remove_aokie_certs(&work_dir) {
            Ok(()) => Ok(json!({"removed": true})),
            Err(e) => Err(CmdError::failed(format!("remove Aokie certs failed: {e}"))),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn remove_certs(&self, _payload: &Value) -> Result<Value, CmdError> {
        Err(CmdError::failed(
            "dongle.removeCerts is only supported on Windows",
        ))
    }

    /// Enumerate actually-connected USB dongles: every plugged-in device that either matches the
    /// compatibility catalog OR already has the WinUSB (aokie) driver bound, annotated so the UI can
    /// tell the user which dongle to pick and whether its driver still needs installing. Windows-only
    /// live enumeration; other targets return an empty list + a note.
    #[cfg(target_os = "windows")]
    fn list_connected_dongles(&self) -> (Vec<Value>, Option<String>) {
        match aokie_dongle::list_devices(true) {
            Ok(devices) => {
                let known: std::collections::HashSet<(u16, u16)> =
                    dongle_catalog::list_known_dongles()
                        .iter()
                        .map(|d| (d.vid, d.pid))
                        .collect();
                let out = devices
                    .into_iter()
                    .filter(|d| {
                        known.contains(&(d.vid, d.pid))
                            || d.driver.to_lowercase().contains("winusb")
                    })
                    .map(|d| {
                        json!({
                            "vid": d.vid,
                            "pid": d.pid,
                            "vidHex": format!("0x{:04X}", d.vid),
                            "pidHex": format!("0x{:04X}", d.pid),
                            "description": d.description,
                            "driver": d.driver,
                            "hardwareId": d.hardware_id,
                            "matchesCatalog": known.contains(&(d.vid, d.pid)),
                            "driverBound": d.driver.to_lowercase().contains("winusb"),
                        })
                    })
                    .collect();
                (out, None)
            }
            Err(e) => (
                Vec::new(),
                Some(format!("live USB enumeration failed: {e}")),
            ),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn list_connected_dongles(&self) -> (Vec<Value>, Option<String>) {
        (
            Vec::new(),
            Some("live USB enumeration is Windows-only".to_string()),
        )
    }

    fn outbox_counts(&self) -> Result<crate::outbox::OutboxCounts, CmdError> {
        self.outbox
            .counts()
            .map_err(|e| CmdError::failed(format!("outbox unavailable: {e}")))
    }
}

/// Canonical `call.current` call object (audit C-02): `callId`/`from`/
/// `state`/`startedAt` — identical keys to the real radio path and the
/// browser mock, so every consumer parses ONE shape.
fn call_json(call: &MockCall) -> Value {
    json!({
        "callId": call.correlation_id,
        "from": call.caller,
        "state": call.state.canonical(),
        "startedAt": call.started_at,
        "turns": call.turns,
    })
}

/// Verify an operator-supplied `callId` names the plugin's CURRENT call.
/// An omitted `callId` acts on the current call (compatibility with flow /
/// desktop callers that predate call identity). A mismatch — or a `callId`
/// with no live call behind it — is the typed `stale_call` error and the
/// phone is NOT touched (audit C-01).
fn check_call_id(provided: Option<&str>, current: Option<&str>) -> Result<(), CmdError> {
    match (provided, current) {
        (None, _) => Ok(()),
        (Some(p), Some(c)) if p == c => Ok(()),
        (Some(p), Some(c)) => Err(CmdError::stale_call(format!(
            "callId {p:?} is not the current call ({c:?})"
        ))),
        (Some(p), None) => Err(CmdError::stale_call(format!(
            "callId {p:?} is stale: there is no current call"
        ))),
    }
}

fn connector_error_line(id: &Value, err: &CmdError) -> String {
    rpc::error_line(
        Some(id),
        rpc::COMMAND_ERROR,
        &err.message,
        Some(json!({"code": err.code, "message": err.message})),
    )
}

/// AOK-CTRL-001: a fresh operation id for an ACCEPTED call control. Returned
/// in the `accepted/queued` command result and carried by the radio thread so
/// an asynchronous failure (`aokie.hardware.error` code `control_failed` /
/// `speak_failed`) correlates back to exactly this request. Completion is
/// confirmed by the call-lifecycle events themselves (answered/ended/turn).
fn operation_id() -> String {
    format!("op_{}", uuid::Uuid::new_v4().simple())
}

/// Payload validation: `null`/missing means "no payload"; objects may
/// only carry the allowed keys. Anything else (arrays, scalars,
/// unknown fields) is rejected — commands must validate defensively.
fn expect_fields(payload: &Value, allowed: &[&str]) -> Result<Map<String, Value>, CmdError> {
    match payload {
        Value::Null => Ok(Map::new()),
        Value::Object(obj) => {
            for key in obj.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(CmdError::failed(format!("unknown payload field: {key}")));
                }
            }
            Ok(obj.clone())
        }
        other => Err(CmdError::failed(format!(
            "payload must be an object, got {}",
            json_type_name(other)
        ))),
    }
}

/// Settings the running radio applies immediately via `RadioControl::Configure`
/// (audit AK-006 `appliedLive`); everything else known takes effect at the
/// next connect (`appliesAtReconnect` — read once at radio spawn).
/// One known setting's contract (audit AOK-CONFIG-002): THE single source
/// for type, bounds and apply-time. `docs/contracts/aokie-settings-schema.v1
/// .json` is generated from this table and test-locked in BOTH repos, so the
/// plugin, the Desktop panel and the FormLogic pack cannot disagree about
/// defaults, options or what applies live versus at reconnect.
pub struct SettingSpec {
    pub key: &'static str,
    pub kind: SettingKind,
    /// true = a running radio applies it immediately (RadioControl::Configure);
    /// false = read once at radio spawn — takes effect at the next connect.
    pub applies_live: bool,
}

pub enum SettingKind {
    Bool,
    Int { min: i64, max: i64 },
    Enum(&'static [&'static str]),
    Str { max_chars: usize },
    EndpointUrl,
}

/// Every known operational setting. Unknown keys stay allowed (the bag is
/// deliberately extensible) but must be scalar and bounded.
pub const SETTING_SPECS: &[SettingSpec] = &[
    SettingSpec { key: "autoAnswer", kind: SettingKind::Bool, applies_live: false },
    SettingSpec { key: "aiReceptionist", kind: SettingKind::Bool, applies_live: false },
    SettingSpec { key: "bargeIn", kind: SettingKind::Bool, applies_live: false },
    SettingSpec { key: "agentHangup", kind: SettingKind::Bool, applies_live: false },
    SettingSpec { key: "reenumerateHwid", kind: SettingKind::Bool, applies_live: false },
    SettingSpec { key: "legacyPairingPin", kind: SettingKind::Bool, applies_live: false },
    SettingSpec { key: "mockCalls", kind: SettingKind::Bool, applies_live: false },
    SettingSpec { key: "bargeSensitivity", kind: SettingKind::Int { min: 50, max: 5000 }, applies_live: false },
    SettingSpec { key: "sttEndpointMs", kind: SettingKind::Int { min: 100, max: 5000 }, applies_live: false },
    // AOK-CTRL-001: seconds of MUTUAL silence before the agent checks in, then
    // (after a second silent window) says goodbye and hangs up. 0 = disabled.
    SettingSpec { key: "maxSilenceSecs", kind: SettingKind::Int { min: 0, max: 600 }, applies_live: false },
    SettingSpec { key: "hfpCodec", kind: SettingKind::Enum(&["auto", "cvsd", "wbs"]), applies_live: false },
    SettingSpec { key: "persona", kind: SettingKind::Str { max_chars: 4000 }, applies_live: true },
    SettingSpec { key: "greeting", kind: SettingKind::Str { max_chars: 1000 }, applies_live: true },
    SettingSpec { key: "ttsVoice", kind: SettingKind::Str { max_chars: 200 }, applies_live: true },
    SettingSpec { key: "aiModel", kind: SettingKind::Str { max_chars: 200 }, applies_live: true },
    SettingSpec { key: "replyMode", kind: SettingKind::Str { max_chars: 200 }, applies_live: false },
    SettingSpec { key: "aiEndpoint", kind: SettingKind::EndpointUrl, applies_live: true },
    SettingSpec { key: "sttEndpoint", kind: SettingKind::EndpointUrl, applies_live: true },
    SettingSpec { key: "ttsEndpoint", kind: SettingKind::EndpointUrl, applies_live: true },
];

fn setting_spec(key: &str) -> Option<&'static SettingSpec> {
    SETTING_SPECS.iter().find(|s| s.key == key)
}

/// Typed validation for settings (audit AK-006/AOK-CONFIG-002): wrong types,
/// out-of-range numbers, bogus enums and unbounded blobs are rejected BEFORE
/// persisting — driven entirely by [`SETTING_SPECS`]. Unknown keys stay
/// allowed but must be scalar and bounded — a typo'd key can't smuggle a
/// megabyte of JSON. `null` always passes: it means "clear this setting".
/// CONSENT-001: true when a speech/AI endpoint URL points at THIS machine
/// (loopback host) — local processing needs no remote-destination consent.
fn is_loopback_endpoint(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let host = if let Some(inner) = authority.strip_prefix('[') {
        inner.split(']').next().unwrap_or(inner)
    } else {
        authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority)
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn validate_setting(key: &str, value: &Value) -> Result<(), CmdError> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    let Some(spec) = setting_spec(key) else {
        return match value {
            Value::Object(_) | Value::Array(_) => Err(CmdError::failed(format!(
                "{key}: objects/arrays are not valid settings values"
            ))),
            Value::String(s) if s.chars().count() > 4000 => Err(CmdError::failed(format!(
                "{key} must be at most 4000 characters"
            ))),
            _ => Ok(()),
        };
    };
    match &spec.kind {
        SettingKind::Bool => match value {
            Value::Bool(_) => Ok(()),
            // Legacy string bools exist in shipped settings.json files.
            Value::String(s) if s == "true" || s == "false" => Ok(()),
            other => Err(CmdError::failed(format!(
                "{key} must be a boolean, got {}",
                json_type_name(other)
            ))),
        },
        SettingKind::Int { min, max } => match value {
            Value::Number(n) => match n.as_i64() {
                Some(n) if (*min..=*max).contains(&n) => Ok(()),
                _ => Err(CmdError::failed(format!(
                    "{key} must be a whole number between {min} and {max}"
                ))),
            },
            other => Err(CmdError::failed(format!(
                "{key} must be a number, got {}",
                json_type_name(other)
            ))),
        },
        SettingKind::Enum(options) => match value {
            Value::String(s) if options.contains(&s.as_str()) => Ok(()),
            _ => Err(CmdError::failed(format!(
                "{key} must be one of: {}",
                options.join(", ")
            ))),
        },
        SettingKind::Str { max_chars } => match value {
            Value::String(s) if s.chars().count() <= *max_chars => Ok(()),
            Value::String(_) => Err(CmdError::failed(format!(
                "{key} must be at most {max_chars} characters"
            ))),
            other => Err(CmdError::failed(format!(
                "{key} must be a string, got {}",
                json_type_name(other)
            ))),
        },
        SettingKind::EndpointUrl => validate_endpoint_setting(key, value),
    }
}

/// Default-OFF auto-answer (audit INT-006/C-15): only an explicit
/// `autoAnswer: true` (bool or the string "true") arms the receptionist.
fn auto_answer_from_settings(settings: &Map<String, Value>) -> bool {
    settings
        .get("autoAnswer")
        .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
        .unwrap_or(false)
}

/// Gate an AI/speech endpoint URL (audit PRIV-001/C-16). Loopback is the
/// design target; a private-LAN endpoint is allowed with a disclosure log;
/// cloud metadata, link-local and unparseable hosts are refused outright;
/// a PUBLIC endpoint must be HTTPS — caller audio/transcripts never leave
/// the machine in cleartext.
fn validate_endpoint_setting(key: &str, value: &Value) -> Result<(), CmdError> {
    use aokie_core::url_classification::{classify_base_url, BaseUrlClassification as C};
    let raw = match value {
        Value::Null => return Ok(()),
        Value::String(s) => s.trim(),
        other => {
            return Err(CmdError::failed(format!(
                "{key} must be a URL string, got {}",
                json_type_name(other)
            )))
        }
    };
    if raw.is_empty() {
        return Ok(()); // clearing the endpoint is always fine
    }
    match classify_base_url(raw) {
        C::Empty | C::Loopback => Ok(()),
        C::Private => {
            eprintln!(
                "[aokie-plugin] {key} points at a private-network host — caller audio/transcripts will leave this machine over the LAN"
            );
            Ok(())
        }
        C::Metadata => Err(CmdError::failed(format!(
            "{key} rejected: cloud-metadata endpoints must never receive caller data"
        ))),
        C::LinkLocal => Err(CmdError::failed(format!(
            "{key} rejected: link-local addresses are almost always a misconfiguration"
        ))),
        C::Invalid => Err(CmdError::failed(format!("{key} rejected: not a valid URL"))),
        C::Public => {
            if raw.starts_with("https://") {
                eprintln!(
                    "[aokie-plugin] {key} points at a public host — caller audio/transcripts will be sent to it"
                );
                Ok(())
            } else {
                Err(CmdError::failed(format!(
                    "{key} rejected: a public endpoint must use https:// (caller audio/transcripts must not travel in cleartext)"
                )))
            }
        }
    }
}

/// Optional string field: missing/null → None; a non-string is an error.
fn optional_str(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, CmdError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(CmdError::failed(format!(
            "field {key} must be a string, got {}",
            json_type_name(other)
        ))),
    }
}

fn require_str(obj: &Map<String, Value>, key: &str) -> Result<String, CmdError> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CmdError::failed(format!("missing or non-string field: {key}")))
}

fn require_u16(obj: &Map<String, Value>, key: &str) -> Result<u16, CmdError> {
    obj.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| CmdError::failed(format!("field {key} must be an integer in 0..=65535")))
}

fn apply_endpoint_env_from_settings(
    settings: &Map<String, Value>,
    setting_key: &str,
    env_key: &str,
) {
    if let Some(ep) = settings.get(setting_key).and_then(Value::as_str) {
        if !ep.trim().is_empty() {
            std::env::set_var(env_key, ep.trim());
        }
    }
}

fn has_receptionist_config_key(obj: &Map<String, Value>) -> bool {
    RECEPTIONIST_CONFIG_KEYS
        .iter()
        .any(|k| obj.contains_key(*k))
}

/// Resolve the spoken greeting from settings. BLANK and MISSING both mean the
/// DEFAULT friendly line — the desktop settings form saves the full settings bag
/// (greeting: "" when untouched) and a flow can push an empty form field; neither
/// must silence the receptionist. Order matters: filter empties THEN default.
fn greeting_from_settings(settings: &Map<String, Value>) -> Option<String> {
    settings
        .get("greeting")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(crate::radio::DEFAULT_GREETING.to_string()))
}

fn string_setting(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bridge::VecSink;
    use crate::outbox::OutboxStatus;

    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn blank_or_missing_greeting_resolves_to_the_default_never_silence() {
        // Missing key (fresh install) → default.
        let empty = Map::new();
        assert_eq!(
            greeting_from_settings(&empty).as_deref(),
            Some(crate::radio::DEFAULT_GREETING)
        );
        // Blank/whitespace (desktop settings form saved the full bag, or a flow
        // pushed an empty form field) → STILL the default, never a silent answer.
        for blank in ["", "   "] {
            let mut settings = Map::new();
            settings.insert("greeting".to_string(), json!(blank));
            assert_eq!(
                greeting_from_settings(&settings).as_deref(),
                Some(crate::radio::DEFAULT_GREETING),
                "greeting {blank:?} must fall back to the default"
            );
        }
        // A real greeting wins.
        let mut settings = Map::new();
        settings.insert("greeting".to_string(), json!("G'day, you've reached Lance."));
        assert_eq!(
            greeting_from_settings(&settings).as_deref(),
            Some("G'day, you've reached Lance.")
        );
    }

    fn request(id: u64, method: &str, params: Value) -> RpcMessage {
        RpcMessage {
            id: Some(json!(id)),
            method: method.to_string(),
            params,
        }
    }

    fn connector_request(id: u64, command: &str, payload: Value) -> RpcMessage {
        request(
            id,
            "connector.request",
            json!({"connectorId": "aokie", "command": command, "payload": payload}),
        )
    }

    fn parse(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn init_health_shutdown_handshake() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();

        let resp = plugin
            .handle_rpc(
                request(
                    1,
                    "plugin.init",
                    json!({"desktopVersion": "1.0.0", "pluginApiVersion": 1, "devMode": false}),
                ),
                &mut sink,
            )
            .unwrap();
        assert_eq!(parse(&resp)["result"]["ok"], json!(true));
        assert!(plugin.initialized);

        let resp = plugin
            .handle_rpc(request(2, "plugin.health", json!({})), &mut sink)
            .unwrap();
        // Truthful health (audit INT-006): real mode with no radio running is
        // DEGRADED, never a blanket ok — the receptionist cannot answer.
        assert_eq!(parse(&resp)["result"]["status"], json!("degraded"));
        assert!(parse(&resp)["result"]["detail"]
            .as_str()
            .unwrap()
            .contains("radio"));

        let resp = plugin
            .handle_rpc(request(3, "plugin.shutdown", json!({})), &mut sink)
            .unwrap();
        assert_eq!(parse(&resp)["result"]["ok"], json!(true));
        assert!(plugin.shutdown_requested);
    }

    #[test]
    fn init_rejects_wrong_api_version() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let resp = plugin
            .handle_rpc(
                request(1, "plugin.init", json!({"pluginApiVersion": 2})),
                &mut sink,
            )
            .unwrap();
        let v = parse(&resp);
        assert_eq!(v["error"]["code"], json!(rpc::INVALID_PARAMS));
        assert!(!plugin.initialized);
    }

    #[test]
    fn unknown_method_is_32601() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let resp = plugin
            .handle_rpc(request(9, "plugin.selfDestruct", json!({})), &mut sink)
            .unwrap();
        assert_eq!(parse(&resp)["error"]["code"], json!(rpc::METHOD_NOT_FOUND));
    }

    #[test]
    fn notifications_get_no_response() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let msg = RpcMessage {
            id: None,
            method: "plugin.health".to_string(),
            params: json!({}),
        };
        assert!(plugin.handle_rpc(msg, &mut sink).is_none());
    }

    #[test]
    fn unknown_command_is_typed_command_failed() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let resp = plugin
            .handle_rpc(
                connector_request(1, "dongle.levitate", json!(null)),
                &mut sink,
            )
            .unwrap();
        let v = parse(&resp);
        assert_eq!(v["error"]["code"], json!(rpc::COMMAND_ERROR));
        assert_eq!(v["error"]["data"]["code"], json!("command_failed"));
        assert!(v["error"]["data"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown command"));
    }

    #[test]
    fn wrong_connector_id_is_connector_missing() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let resp = plugin
            .handle_rpc(
                request(
                    1,
                    "connector.request",
                    json!({"connectorId": "vehicle", "command": "dongle.list"}),
                ),
                &mut sink,
            )
            .unwrap();
        assert_eq!(
            parse(&resp)["error"]["data"]["code"],
            json!("connector_missing")
        );
    }

    #[test]
    fn connector_request_rejects_unknown_fields() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let resp = plugin
            .handle_rpc(
                request(
                    1,
                    "connector.request",
                    json!({"connectorId": "aokie", "command": "dongle.list", "surprise": 1}),
                ),
                &mut sink,
            )
            .unwrap();
        assert_eq!(parse(&resp)["error"]["code"], json!(rpc::INVALID_PARAMS));
    }

    #[test]
    fn request_id_is_echoed_back() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let resp = plugin
            .handle_rpc(
                request(
                    1,
                    "connector.request",
                    json!({"connectorId": "aokie", "command": "dongle.list", "requestId": "trace-7"}),
                ),
                &mut sink,
            )
            .unwrap();
        let v = parse(&resp);
        assert_eq!(v["result"]["ok"], json!(true));
        assert_eq!(v["result"]["requestId"], json!("trace-7"));
    }

    #[test]
    fn dongle_list_serves_catalog_with_source() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let data = plugin
            .dispatch_command("dongle.list", &Value::Null, &mut sink)
            .unwrap();
        // Back-compat: `dongles` is still the compatibility catalog.
        let dongles = data["dongles"].as_array().unwrap();
        assert_eq!(dongles.len(), dongle_catalog::DEFAULT_CATALOG.len());
        assert_eq!(dongles[0]["source"], json!("catalog"));
        assert_eq!(
            dongles[0]["vid"].as_u64().unwrap() as u16,
            dongle_catalog::DEFAULT_CATALOG[0].vid
        );
        // New: live enumeration adds a `connected` array + a `liveEnumeration` flag. On Windows it
        // enumerates real USB devices (empty here with no dongle attached); off-Windows it's a note.
        assert!(data["connected"].is_array());
        assert!(data["liveEnumeration"].is_boolean());
        assert!(data["note"].is_string());
    }

    #[test]
    fn preferred_dongle_round_trips_through_config() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let data = plugin
            .dispatch_command("dongle.getPreferred", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["preferred"], Value::Null);

        plugin
            .dispatch_command(
                "dongle.setPreferred",
                &json!({"vid": 0x0a5c, "pid": 0x21e8}),
                &mut sink,
            )
            .unwrap();
        let data = plugin
            .dispatch_command("dongle.getPreferred", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["preferred"]["vid"], json!(0x0a5c));

        // And it persisted to disk.
        let reloaded = ConfigStore::load(&plugin.data_dir);
        assert_eq!(
            reloaded.config.preferred_dongle,
            Some(PreferredDongle {
                vid: 0x0a5c,
                pid: 0x21e8
            })
        );
    }

    #[test]
    fn set_preferred_validates_payload() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        for bad in [
            json!({"vid": "0a5c", "pid": 1}),
            json!({"vid": 70000, "pid": 1}),
            json!({"vid": 1}),
            json!({"vid": 1, "pid": 2, "extra": 3}),
            json!([1, 2]),
        ] {
            let err = plugin
                .dispatch_command("dongle.setPreferred", &bad, &mut sink)
                .unwrap_err();
            assert_eq!(err.code, "command_failed", "payload: {bad}");
        }
    }

    #[test]
    fn hardware_commands_fail_typed_not_fake_success() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        // dongle.installDriver is wired to the WinUSB installer now. With no vid/pid and no preferred
        // dongle it fails TYPED asking for them (never a fake success); off-Windows it's unsupported.
        let err = plugin
            .dispatch_command("dongle.installDriver", &Value::Null, &mut sink)
            .unwrap_err();
        assert_eq!(err.code, "command_failed");
        #[cfg(target_os = "windows")]
        assert!(err.message.contains("vid"), "got: {}", err.message);
        #[cfg(not(target_os = "windows"))]
        assert!(err.message.contains("Windows"), "got: {}", err.message);

        let err = plugin
            .dispatch_command("dongle.diagnostics", &Value::Null, &mut sink)
            .unwrap_err();
        assert!(err.message.contains("not yet wired to hardware"));
        // Diagnostics surfaces outbox health even in the error message.
        assert!(err.message.contains("dead"));
    }

    #[test]
    fn simulate_requires_dev_mode() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let err = plugin
            .dispatch_command(
                "dongle.diagnostics",
                &json!({"simulate": "call"}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("dev mode"));
        assert!(sink.lines.is_empty());
    }

    #[test]
    fn simulated_call_lifecycle_order_and_idempotency_keys() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();
        let data = plugin
            .dispatch_command(
                "dongle.diagnostics",
                &json!({"simulate": "call"}),
                &mut sink,
            )
            .unwrap();

        // Contract §4 order.
        let expected = [
            crate::contract::events::DONGLE_DETECTED,
            crate::contract::events::DONGLE_READY,
            crate::contract::events::CALL_INCOMING,
            crate::contract::events::CALL_ANSWERED,
            crate::contract::events::CALL_TURN_FINAL,
            crate::contract::events::CALL_TURN_FINAL,
            crate::contract::events::CALL_ENDED,
            crate::contract::events::SMS_RECEIVED,
        ];
        let events: Vec<Value> = sink.lines.iter().map(|l| parse(l)).collect();
        assert_eq!(events.len(), expected.len());
        let corr = data["correlationId"].as_str().unwrap();
        assert!(corr.starts_with("call_"));
        for (i, (line, want)) in events.iter().zip(expected.iter()).enumerate() {
            assert_eq!(line["method"], json!("event.emit"), "event {i}");
            let ev = &line["params"]["event"];
            assert_eq!(ev["name"], json!(*want), "event {i}");
            // One correlation id across the whole scripted sequence.
            assert_eq!(ev["correlationId"], json!(corr), "event {i}");
        }

        // Idempotency keys follow aokie:<corr>:<step>:v1 with the
        // call./aokie. prefixes stripped and turn indexes appended.
        let key = |i: usize| events[i]["params"]["event"]["idempotencyKey"].clone();
        assert_eq!(key(0), json!(format!("aokie:{corr}:dongle.detected:v1")));
        assert_eq!(key(2), json!(format!("aokie:{corr}:incoming:v1")));
        assert_eq!(key(3), json!(format!("aokie:{corr}:answered:v1")));
        assert_eq!(key(4), json!(format!("aokie:{corr}:turn.1.final:v1")));
        assert_eq!(key(5), json!(format!("aokie:{corr}:turn.2.final:v1")));
        assert_eq!(key(6), json!(format!("aokie:{corr}:ended:v1")));
        assert_eq!(key(7), json!(format!("aokie:{corr}:sms.received:v1")));

        // Every scripted step landed in the outbox and was marked sent.
        for ev in &events {
            let k = ev["params"]["event"]["idempotencyKey"].as_str().unwrap();
            assert_eq!(
                plugin.outbox.status_of(k).unwrap(),
                Some(OutboxStatus::Sent),
                "outbox missing {k}"
            );
        }
        assert_eq!(data["outbox"]["sent"], json!(8));

        // The mock call ended; the confirmation SMS seeded a thread.
        assert_eq!(
            plugin.mock.current_call.as_ref().unwrap().state,
            MockCallState::Ended
        );
        assert_eq!(plugin.mock.sms_threads.len(), 1);
    }

    #[test]
    fn call_state_machine_over_simulated_call() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();

        // No call yet.
        let err = plugin
            .dispatch_command("call.answer", &Value::Null, &mut sink)
            .unwrap_err();
        assert!(err.message.contains("no active call"));

        plugin
            .dispatch_command(
                "dongle.diagnostics",
                &json!({"simulate": "call"}),
                &mut sink,
            )
            .unwrap();
        // Scripted call already ended → answer fails typed.
        let err = plugin
            .dispatch_command("call.answer", &Value::Null, &mut sink)
            .unwrap_err();
        assert!(err.message.contains("ended"));

        // Fabricate a FRESH incoming call to drive manually. It must be a new
        // call identity — re-ending the scripted call's correlation would (and
        // now does) trip the outbox key-collision tripwire (AOK-EVENT-001):
        // one call ends once.
        {
            let call = plugin.mock.current_call.as_mut().unwrap();
            call.state = MockCallState::Incoming;
            call.correlation_id = format!("call_{}", uuid::Uuid::new_v4().simple());
        }
        sink.lines.clear();
        let data = plugin
            .dispatch_command("call.answer", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["answered"], json!(true));
        assert_eq!(data["call"]["state"], json!("active"));
        let v = parse(&sink.lines[0]);
        assert_eq!(v["params"]["event"]["name"], json!(crate::contract::events::CALL_ANSWERED));

        let data = plugin
            .dispatch_command("call.current", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["call"]["state"], json!("active"));

        let data = plugin
            .dispatch_command(
                "call.operatorSpeak",
                &json!({"text": "One moment"}),
                &mut sink,
            )
            .unwrap();
        assert_eq!(data["spoken"], json!(true));

        sink.lines.clear();
        let data = plugin
            .dispatch_command("call.hangup", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["ended"], json!(true));
        let v = parse(&sink.lines[0]);
        assert_eq!(v["params"]["event"]["name"], json!(crate::contract::events::CALL_ENDED));

        // Ended call: hangup again fails.
        assert!(plugin
            .dispatch_command("call.hangup", &Value::Null, &mut sink)
            .is_err());
    }

    /// Audit INT-006/C-15: health is COMPUTED, never a constant ok — a build
    /// that cannot speak, or a real-mode process with no radio, says so.
    #[test]
    fn health_reports_real_component_state() {
        let mut plugin = Plugin::ephemeral(true); // dev mode: no radio expected
        let mut sink = VecSink::default();
        let resp = plugin
            .handle_rpc(request(1, "plugin.health", json!({})), &mut sink)
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        let health = &v["result"];
        assert_eq!(health["components"]["voice"], json!(cfg!(feature = "voice")));
        assert_eq!(health["components"]["devMode"], json!(true));
        assert_eq!(health["components"]["radio"]["present"], json!(false));
        assert!(health["components"]["outbox"]["dead"].is_u64());
        if cfg!(feature = "voice") {
            // Dev mode with voice compiled and a clean outbox: genuinely ok.
            assert_eq!(health["status"], json!("ok"));
        } else {
            // No voice output → the receptionist cannot speak → degraded.
            assert_eq!(health["status"], json!("degraded"));
            assert!(health["detail"].as_str().unwrap().contains("voice"));
        }

        // Non-dev with no radio is degraded in EVERY build.
        let mut real = Plugin::ephemeral(false);
        let resp = real
            .handle_rpc(request(2, "plugin.health", json!({})), &mut sink)
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["status"], json!("degraded"));
    }

    /// PROC-001 item 4: the responder (whatever produces replies) is part of
    /// readiness. Default settings = flow mode (host flows reply — the plugin
    /// points at the real check instead of guessing); aiReceptionist=true =
    /// agent mode, whose readiness is the LLM probe's llm_error slot (no radio
    /// in this test → no recorded failure → ready).
    #[test]
    fn health_names_the_responder_mode() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();

        let health = plugin.build_health();
        let responder = &health["components"]["responder"];
        assert_eq!(responder["mode"], json!("flow"), "default = flow responder");
        assert_eq!(responder["ready"], json!(true));
        assert!(
            responder["note"].as_str().unwrap().contains("host flows"),
            "flow mode names where the real readiness check lives"
        );

        plugin
            .dispatch_command("settings.set", &json!({"aiReceptionist": true}), &mut sink)
            .unwrap();
        let health = plugin.build_health();
        let responder = &health["components"]["responder"];
        assert_eq!(responder["mode"], json!("agent"));
        // No radio in dev mode → no probe → no recorded LLM failure → ready.
        assert_eq!(responder["ready"], json!(true));
        assert_eq!(responder["llmError"], Value::Null);
    }

    /// AOK-CONSENT-001: in enforce mode, sensitive commands are refused until
    /// a scoped grant is recorded; consent.set unblocks them; consent.revoke
    /// FORMLOGIC_CONSENT_VERIFY_KEY is process-global: every test that sets
    /// OR depends on its absence takes this lock so parallel runs can't leak
    /// a signing requirement into a legacy-path test.
    fn consent_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// clears the grant + disables auto-answer.
    #[test]
    fn consent_enforce_blocks_then_records_then_revokes() {
        let _env = consent_env_lock();
        std::env::remove_var("FORMLOGIC_CONSENT_VERIFY_KEY");
        let mut plugin = Plugin::ephemeral(false); // non-dev → consent is live
        let mut sink = VecSink::default();

        // Flip to enforce.
        plugin
            .dispatch_command("settings.set", &json!({"consentMode": "enforce"}), &mut sink)
            .unwrap();

        // No grant yet → pairing + sms.send are refused with a consent message.
        let err = plugin
            .dispatch_command("phone.startPairing", &json!({}), &mut sink)
            .unwrap_err();
        assert!(err.message.to_lowercase().contains("consent"), "got: {}", err.message);
        let err = plugin
            .dispatch_command("sms.send", &json!({"to": "+61400000000", "body": "hi"}), &mut sink)
            .unwrap_err();
        assert!(err.message.to_lowercase().contains("consent"), "got: {}", err.message);

        // consent.get reflects the block.
        let g = plugin
            .dispatch_command("consent.get", &json!({}), &mut sink)
            .unwrap();
        assert_eq!(g["mode"], json!("enforce"));
        assert_eq!(g["bluetooth"]["allowed"], json!(false));

        // Record consent for bluetooth + sms.
        let set = plugin
            .dispatch_command(
                "consent.set",
                &json!({"scopes": {"bluetooth": true, "sms": true}, "acceptedBy": "op@example.com"}),
                &mut sink,
            )
            .unwrap();
        assert_eq!(set["recorded"], json!(true));

        // Now allowed: consent.get says so, and pairing no longer fails on
        // consent (it fails later, for lack of a radio in the test process).
        let g = plugin
            .dispatch_command("consent.get", &json!({}), &mut sink)
            .unwrap();
        assert_eq!(g["bluetooth"]["allowed"], json!(true));
        let err = plugin
            .dispatch_command("phone.startPairing", &json!({}), &mut sink)
            .unwrap_err();
        assert!(!err.message.to_lowercase().contains("consent"), "got: {}", err.message);

        // Revoke: grant gone, auto-answer disabled, gate blocks again.
        let rev = plugin
            .dispatch_command("consent.revoke", &json!({}), &mut sink)
            .unwrap();
        assert_eq!(rev["revoked"], json!(true));
        assert_eq!(rev["autoAnswerDisabled"], json!(true));
        let g = plugin
            .dispatch_command("consent.get", &json!({}), &mut sink)
            .unwrap();
        assert_eq!(g["grant"], Value::Null);
        assert_eq!(g["bluetooth"]["allowed"], json!(false));
        assert_eq!(
            plugin.store.config.settings.get("autoAnswer"),
            Some(&json!(false))
        );
    }

    /// Audit INT-006/C-15: auto-answer is opt-in, never assumed.
    #[test]
    fn auto_answer_defaults_off_and_requires_explicit_true() {
        let empty = Map::new();
        assert!(!auto_answer_from_settings(&empty), "default is OFF");
        let mut on = Map::new();
        on.insert("autoAnswer".to_string(), json!(true));
        assert!(auto_answer_from_settings(&on));
        let mut on_str = Map::new();
        on_str.insert("autoAnswer".to_string(), json!("true"));
        assert!(auto_answer_from_settings(&on_str));
        let mut off = Map::new();
        off.insert("autoAnswer".to_string(), json!(false));
        assert!(!auto_answer_from_settings(&off));
    }

    /// Audit AOK-CONFIG-002: SETTING_SPECS is the single source for setting
    /// types/bounds/apply-times, and the shared cross-repo schema fixture
    /// must mirror it exactly — a drift fails this repo's CI until the
    /// fixture is updated in a coordinated two-repo PR set.
    #[test]
    fn settings_schema_fixture_matches_setting_specs() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../docs/contracts/aokie-settings-schema.v1.json"
        ))
        .expect("settings schema fixture parses");
        assert_eq!(fixture["settingsSchemaVersion"], 1);
        let rendered: Vec<Value> = SETTING_SPECS
            .iter()
            .map(|spec| {
                let mut v = serde_json::Map::new();
                v.insert("key".into(), json!(spec.key));
                match &spec.kind {
                    SettingKind::Bool => {
                        v.insert("type".into(), json!("bool"));
                    }
                    SettingKind::Int { min, max } => {
                        v.insert("type".into(), json!("int"));
                        v.insert("min".into(), json!(min));
                        v.insert("max".into(), json!(max));
                    }
                    SettingKind::Enum(options) => {
                        v.insert("type".into(), json!("enum"));
                        v.insert("options".into(), json!(options));
                    }
                    SettingKind::Str { max_chars } => {
                        v.insert("type".into(), json!("string"));
                        v.insert("maxChars".into(), json!(max_chars));
                    }
                    SettingKind::EndpointUrl => {
                        v.insert("type".into(), json!("endpointUrl"));
                    }
                }
                v.insert("appliesLive".into(), json!(spec.applies_live));
                Value::Object(v)
            })
            .collect();
        assert_eq!(
            json!(rendered),
            fixture["settings"],
            "SETTING_SPECS drifted from the shared settings schema fixture"
        );
    }

    /// Audit AK-006: settings are TYPED — wrong types, out-of-range numbers,
    /// bogus enums and unbounded blobs are rejected atomically (one bad key
    /// fails the whole batch, nothing persists), while valid writes report
    /// their apply-state and bump the visible config version.
    #[test]
    fn settings_set_is_typed_versioned_and_atomic() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();

        for (payload, why) in [
            (json!({"autoAnswer": "yes"}), "non-bool bool"),
            (json!({"bargeSensitivity": 9000}), "out of range"),
            (json!({"bargeSensitivity": "500"}), "stringly number"),
            (json!({"sttEndpointMs": 5}), "below range"),
            (json!({"hfpCodec": "mp3"}), "bogus enum"),
            (json!({"persona": "x".repeat(4001)}), "unbounded blob"),
            (json!({"customKey": {"nested": true}}), "non-scalar unknown key"),
            // Atomicity: the valid key must not survive its bad sibling.
            (json!({"greeting": "Hi!", "hfpCodec": "mp3"}), "bad sibling"),
        ] {
            assert!(
                plugin.dispatch_command("settings.set", &payload, &mut sink).is_err(),
                "{why} must be rejected"
            );
        }
        let all = plugin.dispatch_command("settings.get", &json!({}), &mut sink).unwrap();
        assert_eq!(all["configVersion"], 0, "rejected writes must not bump the version");
        assert!(all["settings"].get("greeting").is_none(), "bad batch persisted its valid key");
        assert_eq!(all["configQuarantined"], false);

        // Valid writes: version bumps, apply-state is truthful (no radio in
        // ephemeral mode, so even live keys wait for the next connect).
        let res = plugin
            .dispatch_command(
                "settings.set",
                &json!({"greeting": "Hi!", "bargeSensitivity": 500, "autoAnswer": true, "customFlag": true}),
                &mut sink,
            )
            .unwrap();
        assert_eq!(res["configVersion"], 1);
        assert_eq!(res["appliedLive"], json!([]));
        let mut reconnect: Vec<String> = res["appliesAtReconnect"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        reconnect.sort();
        assert_eq!(reconnect, ["autoAnswer", "bargeSensitivity", "greeting"]);
        assert!(plugin
            .dispatch_command("settings.set", &json!({"hfpCodec": "wbs"}), &mut sink)
            .is_ok());
        let all = plugin.dispatch_command("settings.get", &json!({}), &mut sink).unwrap();
        assert_eq!(all["configVersion"], 2);
    }

    /// Audit PRIV-001/C-16: AI/speech endpoints are classified BEFORE being
    /// persisted — unsafe destinations for caller audio/transcripts never
    /// reach the config.
    #[test]
    fn settings_set_rejects_unsafe_endpoints() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();

        // Loopback (the design target) and clearing are fine.
        plugin
            .dispatch_command(
                "settings.set",
                &json!({"sttEndpoint": "http://127.0.0.1:17920/v1/audio/transcriptions"}),
                &mut sink,
            )
            .unwrap();
        plugin
            .dispatch_command("settings.set", &json!({"sttEndpoint": ""}), &mut sink)
            .unwrap();
        // Public HTTPS is allowed (with disclosure); public HTTP is not.
        plugin
            .dispatch_command(
                "settings.set",
                &json!({"aiEndpoint": "https://api.example.com/v1/chat/completions"}),
                &mut sink,
            )
            .unwrap();
        for (key, url, why) in [
            ("aiEndpoint", "http://api.example.com/v1", "cleartext public"),
            ("aiEndpoint", "http://169.254.169.254/latest/meta-data", "metadata"),
            ("ttsEndpoint", "http://169.254.7.9:9000/tts", "link-local"),
        ] {
            let err = plugin
                .dispatch_command("settings.set", &json!({ key: url }), &mut sink)
                .unwrap_err();
            assert_eq!(err.code, crate::contract::errors::COMMAND_FAILED, "{why}");
            // Rejected values are NEVER persisted.
            assert!(
                plugin.store.config.settings.get(key).map(|v| v != &json!(url)).unwrap_or(true),
                "{why}: rejected URL must not be stored"
            );
        }
    }

    /// Audit INT-003: `plugin.init` feature negotiation flips ack mode, and
    /// an `event.ack` NOTIFICATION (no id, no response) marks the outbox row
    /// delivered — the ack path end to end at the dispatch layer.
    #[test]
    fn init_features_enable_ack_mode_and_event_ack_marks_sent() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();
        assert!(!plugin.ack_mode);

        // Handshake WITHOUT the feature: legacy mode.
        let resp = plugin
            .handle_rpc(
                request(1, "plugin.init", json!({"pluginApiVersion": 1})),
                &mut sink,
            )
            .unwrap();
        assert!(resp.contains("\"ok\":true"));
        assert!(!plugin.ack_mode);

        // Handshake WITH eventAck: ack mode on.
        let resp = plugin
            .handle_rpc(
                request(
                    2,
                    "plugin.init",
                    json!({"pluginApiVersion": 1, "features": ["eventAck"]}),
                ),
                &mut sink,
            )
            .unwrap();
        assert!(resp.contains("\"ok\":true"));
        assert!(plugin.ack_mode);

        // An outboxed emission now awaits the ack…
        let ev = aokie_event(
            crate::contract::events::CALL_INCOMING,
            "call_ack",
            json!({"from": "x"}),
        );
        crate::event_bridge::emit_event(
            &mut sink,
            &plugin.outbox,
            &ev,
            false,
            crate::event_bridge::EmitMode::for_host(plugin.ack_mode, plugin.dev_mode || crate::event_bridge::legacy_host_allowed()),
        )
        .unwrap();
        assert_eq!(
            plugin.outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Pending)
        );

        // …and the host's event.ack notification (id-less; must produce NO
        // response line) marks it sent.
        let ack = RpcMessage {
            id: None,
            method: "event.ack".to_string(),
            params: json!({"idempotencyKey": ev.idempotency_key}),
        };
        assert!(plugin.handle_rpc(ack, &mut sink).is_none());
        assert_eq!(
            plugin.outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );

        // An ack for an unknown key is harmless (idempotent, still silent).
        let stray = RpcMessage {
            id: None,
            method: "event.ack".to_string(),
            params: json!({"idempotencyKey": "aokie:nope:incoming:v1"}),
        };
        assert!(plugin.handle_rpc(stray, &mut sink).is_none());
    }

    /// AOK-DUR-001 item 3: a PRODUCTION (non-dev) host without eventAck is
    /// unhealthy — health names the incompatibility and the outbox component
    /// reports ackMode — while an ack-capable host clears the reason.
    #[test]
    fn non_ack_production_host_degrades_health() {
        let mut plugin = Plugin::ephemeral(false); // non-dev = production posture
        let mut sink = VecSink::default();
        plugin
            .handle_rpc(
                request(1, "plugin.init", json!({"pluginApiVersion": 1})),
                &mut sink,
            )
            .unwrap();
        let health = plugin.build_health();
        assert_eq!(health["status"], json!("degraded"));
        assert!(
            health["detail"].as_str().unwrap_or("").contains("eventAck"),
            "names the incompatible host: {}",
            health["detail"]
        );
        assert_eq!(health["components"]["outbox"]["ackMode"], json!(false));

        // The same host WITH eventAck: no ack-related reason.
        plugin
            .handle_rpc(
                request(2, "plugin.init", json!({"pluginApiVersion": 1, "features": ["eventAck"]})),
                &mut sink,
            )
            .unwrap();
        let health = plugin.build_health();
        assert!(
            !health["detail"].as_str().unwrap_or("").contains("eventAck"),
            "ack-capable host is not flagged: {}",
            health["detail"]
        );
        assert_eq!(health["components"]["outbox"]["ackMode"], json!(true));
    }

    /// Audit C-01: call controls accept an optional `callId`; a stale one
    /// (wrong call, or no call at all) is the typed `stale_call` error and
    /// leaves the call state untouched.
    #[test]
    fn call_controls_verify_call_id() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();

        // A callId with no call behind it is stale, not "no active call".
        let err = plugin
            .dispatch_command("call.answer", &json!({"callId": "call_gone"}), &mut sink)
            .unwrap_err();
        assert_eq!(err.code, crate::contract::errors::STALE_CALL);

        plugin
            .dispatch_command(
                "dongle.diagnostics",
                &json!({"simulate": "call"}),
                &mut sink,
            )
            .unwrap();
        // Fresh call identity for the manual drive — re-ending the scripted
        // call's correlation would trip the key-collision tripwire (one call
        // ends once, AOK-EVENT-001).
        {
            let call = plugin.mock.current_call.as_mut().unwrap();
            call.state = MockCallState::Incoming;
            call.correlation_id = format!("call_{}", uuid::Uuid::new_v4().simple());
        }
        let current = plugin.mock_call_id().unwrap();

        // Wrong callId → stale_call, state untouched.
        for command in ["call.answer", "call.reject", "call.hangup"] {
            let err = plugin
                .dispatch_command(command, &json!({"callId": "call_stale"}), &mut sink)
                .unwrap_err();
            assert_eq!(err.code, crate::contract::errors::STALE_CALL, "{command}");
            assert_eq!(
                plugin.mock.current_call.as_ref().unwrap().state,
                MockCallState::Incoming,
                "{command} must not touch a call it failed to target"
            );
        }
        // Non-string callId is a validation failure, not a stale call.
        let err = plugin
            .dispatch_command("call.answer", &json!({"callId": 7}), &mut sink)
            .unwrap_err();
        assert_eq!(err.code, crate::contract::errors::COMMAND_FAILED);

        // The RIGHT callId answers the call (this is the exact payload the
        // Live Call screen sends — audit C-01's broken case).
        let data = plugin
            .dispatch_command("call.answer", &json!({"callId": current}), &mut sink)
            .unwrap();
        assert_eq!(data["answered"], json!(true));

        // operatorSpeak also honours the guard.
        let err = plugin
            .dispatch_command(
                "call.operatorSpeak",
                &json!({"text": "hello", "callId": "call_stale"}),
                &mut sink,
            )
            .unwrap_err();
        assert_eq!(err.code, crate::contract::errors::STALE_CALL);
        let data = plugin
            .dispatch_command(
                "call.operatorSpeak",
                &json!({"text": "hello", "callId": current}),
                &mut sink,
            )
            .unwrap();
        assert_eq!(data["spoken"], json!(true));

        // And hangup with the right id ends it.
        let data = plugin
            .dispatch_command("call.hangup", &json!({"callId": current}), &mut sink)
            .unwrap();
        assert_eq!(data["ended"], json!(true));
    }

    /// Audit C-02: `call.current` returns ONE canonical call object —
    /// `callId`/`from`/`state`/`startedAt` with contract state values —
    /// from the mock path (the radio path mirrors it from RadioStatus).
    #[test]
    fn call_current_returns_the_canonical_shape() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();

        // No call → explicit null.
        let data = plugin
            .dispatch_command("call.current", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["call"], Value::Null);

        plugin
            .dispatch_command(
                "dongle.diagnostics",
                &json!({"simulate": "call"}),
                &mut sink,
            )
            .unwrap();
        plugin.mock.current_call.as_mut().unwrap().state = MockCallState::Incoming;

        let data = plugin
            .dispatch_command("call.current", &Value::Null, &mut sink)
            .unwrap();
        let call = &data["call"];
        assert!(call["callId"].as_str().unwrap().starts_with("call_"));
        assert_eq!(call["from"], json!("+61412345678"));
        // Internal "incoming" maps to the contract's "ringing".
        assert_eq!(call["state"], json!(crate::contract::call_state::RINGING));
        assert!(call["startedAt"].as_str().is_some());
        // The legacy keys are GONE — one shape only.
        assert!(call.get("correlationId").is_none());
        assert!(call.get("caller").is_none());
        assert!(call.get("active").is_none());
    }

    /// AOK-CTRL-001: RADIO-backed call controls report ACCEPTANCE (accepted/
    /// queued + operationId), never the final verb — the phone hasn't acted
    /// when the result returns. The queued RadioControl carries the SAME
    /// operation id so an asynchronous `control_failed` correlates back, and
    /// `call.operatorSpeak` is refused typed while the running radio's
    /// in-plugin agent owns replies (the radio would drop it).
    #[test]
    fn radio_call_controls_report_acceptance_not_final_verbs() {
        let mut plugin = Plugin::ephemeral(true);
        let (handle, control_rx) = crate::radio::RadioHandle::test_handle();
        *handle.status.current_call_id.lock().unwrap() = Some("call_live1".to_string());
        plugin.radio = Some(handle);
        let mut sink = VecSink::default();

        let data = plugin
            .dispatch_command("call.answer", &json!({"callId": "call_live1"}), &mut sink)
            .unwrap();
        assert_eq!(data["accepted"], json!(true));
        assert_eq!(data["queued"], json!(true));
        let op = data["operationId"].as_str().unwrap().to_string();
        assert!(op.starts_with("op_"), "{op}");
        assert!(
            data.get("answered").is_none(),
            "the final verb must wait for the phone's confirmation event: {data}"
        );
        match control_rx.try_recv().unwrap() {
            crate::radio::RadioControl::Answer { op: sent } => {
                assert_eq!(sent.as_deref(), Some(op.as_str()))
            }
            _ => panic!("expected the Answer control"),
        }

        let data = plugin
            .dispatch_command("call.hangup", &json!({"callId": "call_live1"}), &mut sink)
            .unwrap();
        assert_eq!(data["accepted"], json!(true));
        assert!(data.get("ended").is_none(), "{data}");
        assert!(matches!(
            control_rx.try_recv().unwrap(),
            crate::radio::RadioControl::Hangup { op: Some(_) }
        ));

        let data = plugin
            .dispatch_command("call.reject", &json!({"callId": "call_live1"}), &mut sink)
            .unwrap();
        assert_eq!(data["accepted"], json!(true));
        assert!(data.get("rejected").is_none(), "{data}");
        assert!(matches!(
            control_rx.try_recv().unwrap(),
            crate::radio::RadioControl::Reject { op: Some(_) }
        ));

        // operatorSpeak: voice builds accept (queued, op id) unless the agent
        // owns replies; non-voice builds refuse outright (INT-006 gate).
        let speak = plugin.dispatch_command(
            "call.operatorSpeak",
            &json!({"text": "One moment", "callId": "call_live1"}),
            &mut sink,
        );
        #[cfg(feature = "voice")]
        {
            let data = speak.unwrap();
            assert_eq!(data["accepted"], json!(true));
            assert!(data.get("spoken").is_none(), "{data}");
            assert!(matches!(
                control_rx.try_recv().unwrap(),
                crate::radio::RadioControl::Speak { op: Some(_), .. }
            ));

            // Agent owns replies → typed refusal, nothing queued.
            plugin
                .radio
                .as_ref()
                .unwrap()
                .status
                .agent_enabled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let err = plugin
                .dispatch_command(
                    "call.operatorSpeak",
                    &json!({"text": "One moment", "callId": "call_live1"}),
                    &mut sink,
                )
                .unwrap_err();
            assert!(err.message.contains("owns replies"), "{}", err.message);
            assert!(control_rx.try_recv().is_err(), "nothing may be queued");
        }
        #[cfg(not(feature = "voice"))]
        {
            let err = speak.unwrap_err();
            assert!(err.message.contains("no voice output"), "{}", err.message);
        }

        // The callId guard still applies to accepted controls.
        let err = plugin
            .dispatch_command("call.answer", &json!({"callId": "call_stale"}), &mut sink)
            .unwrap_err();
        assert_eq!(err.code, "stale_call");
    }

    // ---- CONSENT-001: enforce-by-default + signed grants + destinations ----

    #[test]
    fn consent_enforce_is_the_default_and_denies_sensitive_commands() {
        // A REAL-mode plugin with no consentMode setting and no grant must
        // DENY sensitive commands (production default = enforce, never warn).
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        let err = plugin
            .dispatch_command(
                "sms.send",
                &json!({"to": "+61432123456", "body": "hi"}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("consent"), "{}", err.message);
        assert!(sink.lines.is_empty(), "no event for a consent-denied send");
    }

    #[test]
    fn consent_set_requires_signed_envelope_under_a_signing_desktop() {
        use base64::Engine as _;
        use ed25519_dalek::Signer as _;
        let _env = consent_env_lock();
        let key = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
        let pub_b64 = base64::engine::general_purpose::STANDARD
            .encode(key.verifying_key().to_bytes());
        std::env::set_var("FORMLOGIC_CONSENT_VERIFY_KEY", &pub_b64);

        let dir = std::env::temp_dir().join(format!(
            "aokie-consent-conn-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut plugin = Plugin::new(false, dir.clone()).unwrap();
        let mut sink = VecSink::default();

        // A plain (unsigned) consent.set is refused outright.
        let err = plugin
            .dispatch_command(
                "consent.set",
                &json!({"scopes": {"bluetooth": true, "sms": true}}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("signs consent grants"), "{}", err.message);

        // A correctly signed envelope records + satisfies the gate.
        let grant = crate::consent::ConsentGrant {
            version: crate::consent::CURRENT_CONSENT_VERSION,
            scopes: crate::consent::ConsentScopes {
                bluetooth: true,
                sms: true,
                transcription: true,
                contacts: false,
                recording: false,
                retention_days: Some(90),
                destinations: vec!["https://api.example.com/v1".into()],
            },
            accepted_at: "2026-07-12T00:00:00Z".into(),
            accepted_by: Some("op@example.com".into()),
            expires_at: Some("2027-07-12T00:00:00Z".into()),
            signature: None,
        };
        let payload = serde_json::to_vec(&grant).unwrap();
        let sig = key.sign(&payload);
        let envelope = json!({
            "format": 1,
            "alg": "Ed25519",
            "keyId": "desktop-test",
            "payloadB64": base64::engine::general_purpose::STANDARD.encode(&payload),
            "signature": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes()),
        });
        let out = plugin
            .dispatch_command("consent.set", &json!({"envelope": envelope}), &mut sink)
            .unwrap();
        assert_eq!(out["recorded"], json!(true));
        assert_eq!(out["signed"], json!(true));

        // A TAMPERED envelope (payload swapped for wider scopes) is refused.
        let mut wider = grant.clone();
        wider.scopes.recording = true;
        let forged = json!({
            "format": 1,
            "alg": "Ed25519",
            "keyId": "desktop-test",
            "payloadB64": base64::engine::general_purpose::STANDARD
                .encode(serde_json::to_vec(&wider).unwrap()),
            "signature": envelope["signature"],
        });
        let err = plugin
            .dispatch_command("consent.set", &json!({"envelope": forged}), &mut sink)
            .unwrap_err();
        assert!(err.message.contains("verification failed"), "{}", err.message);

        // CONSENT-001 destinations: with the signed grant recorded, a remote
        // endpoint NOT in grant.destinations is refused at settings.set…
        let err = plugin
            .dispatch_command(
                "settings.set",
                &json!({"aiEndpoint": "https://rogue.example.net/v1"}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("not a consented destination"), "{}", err.message);
        // …a consented one is accepted, and loopback needs no destination grant.
        plugin
            .dispatch_command(
                "settings.set",
                &json!({"aiEndpoint": "https://api.example.com/v1"}),
                &mut sink,
            )
            .unwrap();
        plugin
            .dispatch_command(
                "settings.set",
                &json!({"sttEndpoint": "http://127.0.0.1:17920/v1"}),
                &mut sink,
            )
            .unwrap();

        // Revocation stops enforcement satisfaction immediately (no restart):
        // the next sensitive command denies again.
        plugin
            .dispatch_command("consent.revoke", &Value::Null, &mut sink)
            .unwrap();
        sink.lines.clear();
        let err = plugin
            .dispatch_command(
                "sms.send",
                &json!({"to": "+61432123456", "body": "hi"}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("consent"), "{}", err.message);

        std::env::remove_var("FORMLOGIC_CONSENT_VERIFY_KEY");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loopback_endpoint_detection() {
        assert!(is_loopback_endpoint("http://127.0.0.1:17920/v1"));
        assert!(is_loopback_endpoint("http://localhost:8080"));
        assert!(is_loopback_endpoint("http://[::1]:9000/x"));
        assert!(!is_loopback_endpoint("https://api.example.com/v1"));
        assert!(!is_loopback_endpoint("http://192.168.1.10:8080"));
        assert!(!is_loopback_endpoint("http://localhost.evil.com/v1"));
    }

    #[test]
    fn sms_send_real_mode_without_radio_is_a_typed_outage_never_queued() {
        // FL-CONN-001: the canonical fake success — a real-mode plugin whose
        // radio never started must NOT report an SMS as queued (nor emit
        // sms.sent); the caller was promised a message that could never exist.
        // consentMode=warn (the explicit dev override) so the RADIO outage is
        // what surfaces — the enforce-default consent denial has its own test.
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        plugin
            .dispatch_command("settings.set", &json!({"consentMode": "warn"}), &mut sink)
            .unwrap();
        let err = plugin
            .dispatch_command(
                "sms.send",
                &json!({"to": "+61432123456", "body": "See you at 9:30"}),
                &mut sink,
            )
            .unwrap_err();
        assert_eq!(err.code, "command_failed");
        assert!(err.message.contains("radio is not running"));
        assert!(err.message.contains("not performed"));
        assert!(sink.lines.is_empty(), "no sms.sent event for an unsendable message");

        // The dev-only mock thread reads fail typed in real mode too.
        let err = plugin
            .dispatch_command("sms.threads", &Value::Null, &mut sink)
            .unwrap_err();
        assert!(err.message.contains("radio is not running"));
    }

    #[test]
    fn sms_send_validates_and_emits_outboxed_event() {
        let mut plugin = Plugin::ephemeral(true); // dev mode: the explicit simulator
        let mut sink = VecSink::default();

        let err = plugin
            .dispatch_command(
                "sms.send",
                &json!({"to": "DROP TABLE", "body": "x"}),
                &mut sink,
            )
            .unwrap_err();
        assert_eq!(err.code, "command_failed");
        let err = plugin
            .dispatch_command(
                "sms.send",
                &json!({"to": "+61432123456", "body": ""}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("empty"));
        assert!(sink.lines.is_empty());

        let data = plugin
            .dispatch_command(
                "sms.send",
                &json!({"to": "+61432123456", "body": "See you at 9:30"}),
                &mut sink,
            )
            .unwrap();
        assert_eq!(data["status"], json!("queued"));
        assert_eq!(data["simulated"], json!(true), "dev-mode results are stamped simulated");
        let message_id = data["messageId"].as_str().unwrap();
        assert!(message_id.starts_with("sms_"));

        let v = parse(&sink.lines[0]);
        assert_eq!(v["params"]["event"]["name"], json!(crate::contract::events::SMS_SENT));
        assert_eq!(v["params"]["event"]["correlationId"], json!(message_id));
        let key = v["params"]["event"]["idempotencyKey"].as_str().unwrap();
        assert_eq!(key, format!("aokie:{message_id}:sms.sent:v1"));
        assert_eq!(
            plugin.outbox.status_of(key).unwrap(),
            Some(OutboxStatus::Sent)
        );

        // Thread bookkeeping.
        let data = plugin
            .dispatch_command("sms.threads", &Value::Null, &mut sink)
            .unwrap();
        let threads = data["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 1);
        let thread_id = threads[0]["id"].as_str().unwrap().to_string();
        let data = plugin
            .dispatch_command("sms.thread", &json!({"threadId": thread_id}), &mut sink)
            .unwrap();
        assert_eq!(data["messages"][0]["direction"], json!("out"));

        let err = plugin
            .dispatch_command("sms.thread", &json!({"threadId": "thread_999"}), &mut sink)
            .unwrap_err();
        assert!(err.message.contains("unknown thread"));
    }

    #[test]
    fn settings_get_set_round_trip() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();

        let data = plugin
            .dispatch_command("settings.get", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["settings"], json!({}));

        plugin
            .dispatch_command("settings.set", &json!({"mockCalls": true}), &mut sink)
            .unwrap();
        let data = plugin
            .dispatch_command("settings.get", &json!({"key": "mockCalls"}), &mut sink)
            .unwrap();
        assert_eq!(data["value"], json!(true));

        // Persisted.
        let reloaded = ConfigStore::load(&plugin.data_dir);
        assert_eq!(
            reloaded.config.settings.get("mockCalls"),
            Some(&json!(true))
        );

        // Invalid payloads.
        assert!(plugin
            .dispatch_command("settings.set", &json!({}), &mut sink)
            .is_err());
        assert!(plugin
            .dispatch_command("settings.set", &json!(null), &mut sink)
            .is_err());
    }

    #[test]
    fn endpoint_settings_map_to_radio_env_vars() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        std::env::remove_var("AOKIE_STT_ENDPOINT");
        std::env::remove_var("AOKIE_TTS_ENDPOINT");

        let mut settings = Map::new();
        settings.insert(
            "sttEndpoint".to_string(),
            json!("  http://127.0.0.1:17920/v1/audio/transcriptions  "),
        );
        settings.insert("ttsEndpoint".to_string(), json!(""));

        apply_endpoint_env_from_settings(&settings, "sttEndpoint", "AOKIE_STT_ENDPOINT");
        apply_endpoint_env_from_settings(&settings, "ttsEndpoint", "AOKIE_TTS_ENDPOINT");

        assert_eq!(
            std::env::var("AOKIE_STT_ENDPOINT").unwrap(),
            "http://127.0.0.1:17920/v1/audio/transcriptions"
        );
        assert!(std::env::var("AOKIE_TTS_ENDPOINT").is_err());

        std::env::remove_var("AOKIE_STT_ENDPOINT");
        std::env::remove_var("AOKIE_TTS_ENDPOINT");
    }

    #[test]
    fn stt_and_tts_endpoint_settings_trigger_live_configure() {
        let obj = json!({
            "sttEndpoint": "http://127.0.0.1:17920/v1/audio/transcriptions",
            "ttsEndpoint": "http://127.0.0.1:17920/v1/audio/speech"
        })
        .as_object()
        .unwrap()
        .clone();

        assert!(has_receptionist_config_key(&obj));
        assert_eq!(
            string_setting(&obj, "sttEndpoint").as_deref(),
            Some("http://127.0.0.1:17920/v1/audio/transcriptions")
        );
        assert_eq!(
            string_setting(&obj, "ttsEndpoint").as_deref(),
            Some("http://127.0.0.1:17920/v1/audio/speech")
        );

        let obj = json!({"mockCalls": true}).as_object().unwrap().clone();
        assert!(!has_receptionist_config_key(&obj));
    }

    #[test]
    fn phone_pairing_mocks_are_dev_only_and_stamped() {
        let mut plugin = Plugin::ephemeral(true); // dev mode: the explicit simulator
        let mut sink = VecSink::default();

        let data = plugin
            .dispatch_command("phone.status", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["paired"], json!(false));

        let data = plugin
            .dispatch_command("phone.startPairing", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["status"], json!("pairing"));
        assert_eq!(data["simulated"], json!(true));
        assert!(data["sessionId"].as_str().unwrap().starts_with("pair_"));
        let v = parse(&sink.lines[0]);
        assert_eq!(
            v["params"]["event"]["name"],
            json!(crate::contract::events::PHONE_PAIRING_STARTED)
        );
        assert_eq!(v["params"]["event"]["data"]["simulated"], json!(true));

        let data = plugin
            .dispatch_command("phone.stopPairing", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["stopped"], json!(true));

        let data = plugin
            .dispatch_command("phone.listPaired", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["devices"], json!([]));
    }

    #[test]
    fn phone_pairing_real_mode_without_radio_fails_typed() {
        // FL-CONN-001 + AOK-BT-001: a real-mode plugin whose radio never started
        // must not report a pairing session the phone can never see, and neither
        // start nor stop pairing can silently succeed. consentMode=warn so the
        // radio outage (not the enforce-default consent denial) is under test.
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();
        plugin
            .dispatch_command("settings.set", &json!({"consentMode": "warn"}), &mut sink)
            .unwrap();

        let err = plugin
            .dispatch_command("phone.startPairing", &Value::Null, &mut sink)
            .unwrap_err();
        assert!(err.message.contains("radio is not running"));
        assert!(sink.lines.is_empty(), "no phone.pairing.started event for a dead radio");

        // AOK-BT-001: stopPairing without a radio is a typed outage (was previously
        // a "nothing was stopped" FL-CONN-001 stub — now unified with the gate).
        let err = plugin
            .dispatch_command("phone.stopPairing", &Value::Null, &mut sink)
            .unwrap_err();
        assert!(err.message.contains("radio is not running"));

        // removePaired is likewise gated on a running radio.
        let err = plugin
            .dispatch_command("phone.removePaired", &json!({"address": "00:11:22:33:44:55"}), &mut sink)
            .unwrap_err();
        assert!(err.message.contains("radio is not running"));

        // Reads stay honest: config-backed status still answers (source: config),
        // and reports the radio is not in a pairing window.
        let data = plugin
            .dispatch_command("phone.status", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["source"], json!("config"));
        assert_eq!(data["pairingOpen"], json!(false));

        // PAIR-001: confirmPairing is gated on a running radio too.
        let err = plugin
            .dispatch_command(
                "phone.confirmPairing",
                &json!({"address": "00:11:22:33:44:55", "accept": true}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("radio is not running"));
    }

    #[test]
    fn phone_confirm_pairing_dev_mode_has_nothing_pending_and_validates_payload() {
        // PAIR-001: dev mode never holds an SSP confirmation, so the command is
        // a typed failure — the UI can't "confirm" a phantom device. A missing
        // or non-boolean `accept` is rejected before any radio interaction.
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();

        let err = plugin
            .dispatch_command(
                "phone.confirmPairing",
                &json!({"address": "00:11:22:33:44:55", "accept": true}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("no pairing confirmation is pending"));

        let err = plugin
            .dispatch_command(
                "phone.confirmPairing",
                &json!({"address": "00:11:22:33:44:55"}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("boolean `accept`"));

        let err = plugin
            .dispatch_command(
                "phone.confirmPairing",
                &json!({"address": "00:11:22:33:44:55", "accept": "yes"}),
                &mut sink,
            )
            .unwrap_err();
        assert!(err.message.contains("boolean `accept`"));
    }

    #[test]
    fn phone_start_pairing_rejects_unknown_fields_but_allows_seconds() {
        // AOK-BT-001: {seconds} is the only accepted field (clamped by the handler).
        let mut plugin = Plugin::ephemeral(true); // dev mode so no radio is required
        let mut sink = VecSink::default();

        // A stray field is refused by the allowlist.
        let err = plugin
            .dispatch_command("phone.startPairing", &json!({"forever": true}), &mut sink)
            .unwrap_err();
        assert!(err.message.contains("unknown payload field"));

        // {seconds} is accepted (dev mode returns the simulated stub).
        let data = plugin
            .dispatch_command("phone.startPairing", &json!({"seconds": 60}), &mut sink)
            .unwrap();
        assert_eq!(data["status"], json!("pairing"));
        assert_eq!(data["simulated"], json!(true));
    }
}
