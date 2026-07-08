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

/// Typed connector-level error, surfaced as a JSON-RPC error with
/// `error.data = {code, message}` (connector-response.schema.json
/// codes; the plugin only ever produces `command_failed` and — for
/// a mis-routed connector id — `connector_missing`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdError {
    pub code: &'static str,
    pub message: String,
}

impl CmdError {
    pub fn failed(message: impl Into<String>) -> Self {
        CmdError {
            code: "command_failed",
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
    pub initialized: bool,
    pub shutdown_requested: bool,
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
            initialized: false,
            shutdown_requested: false,
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
            initialized: false,
            shutdown_requested: false,
        }
    }

    /// Handle one parsed protocol message. Returns the response line
    /// for requests, `None` for notifications (which get no answer).
    pub fn handle_rpc(&mut self, msg: RpcMessage, sink: &mut dyn Sink) -> Option<String> {
        let id = msg.id.clone()?; // notifications: process nothing, answer nothing
        match msg.method.as_str() {
            "plugin.init" => Some(self.handle_init(&id, &msg.params)),
            "plugin.health" => Some(rpc::success_line(&id, json!({"status": "ok"}))),
            "plugin.shutdown" => {
                self.shutdown_requested = true;
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
        }
        self.initialized = true;
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
                message: format!("unknown connector: {connector_id:?} (this plugin serves \"aokie\")"),
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
                        Err(CmdError::failed(format!(
                            "dongle.diagnostics is not yet wired to hardware (outbox: {} pending, {} failed, {} dead)",
                            c.pending, c.failed, c.dead
                        )))
                    }
                }
            }
            "phone.status" => {
                expect_fields(payload, &[])?;
                let device = self.store.config.paired_devices.first();
                Ok(json!({
                    "paired": device.is_some(),
                    "device": device,
                    "source": "config",
                }))
            }
            "phone.startPairing" => {
                expect_fields(payload, &[])?;
                let session_id = format!("pair_{}", uuid::Uuid::new_v4().simple());
                let ev = aokie_event(
                    "aokie.phone.pairing_started",
                    &session_id,
                    json!({"at": now_iso8601()}),
                );
                emit_event(sink, &self.outbox, &ev, false).map_err(CmdError::failed)?;
                Ok(json!({"sessionId": session_id, "status": "pairing"}))
            }
            "phone.stopPairing" => {
                expect_fields(payload, &["sessionId"])?;
                Ok(json!({"stopped": true}))
            }
            "phone.listPaired" => {
                expect_fields(payload, &[])?;
                Ok(json!({"devices": self.store.config.paired_devices}))
            }
            "call.current" => {
                expect_fields(payload, &[])?;
                Ok(json!({"call": self.mock.current_call.as_ref().map(call_json)}))
            }
            "call.answer" => {
                expect_fields(payload, &[])?;
                let call = self.require_call(&[MockCallState::Incoming], "call.answer")?;
                call.state = MockCallState::Active;
                let (corr, snapshot) = {
                    let c = self.mock.current_call.as_ref().unwrap();
                    (c.correlation_id.clone(), call_json(c))
                };
                let ev = aokie_event("aokie.call.answered", &corr, json!({"at": now_iso8601()}));
                emit_event(sink, &self.outbox, &ev, false).map_err(CmdError::failed)?;
                Ok(json!({"answered": true, "call": snapshot}))
            }
            "call.reject" => {
                expect_fields(payload, &[])?;
                let call = self.require_call(&[MockCallState::Incoming], "call.reject")?;
                call.state = MockCallState::Ended;
                let corr = call.correlation_id.clone();
                let ev = aokie_event("aokie.call.rejected", &corr, json!({"at": now_iso8601()}));
                emit_event(sink, &self.outbox, &ev, false).map_err(CmdError::failed)?;
                Ok(json!({"rejected": true}))
            }
            "call.hangup" => {
                expect_fields(payload, &[])?;
                let call = self.require_call(
                    &[MockCallState::Incoming, MockCallState::Active],
                    "call.hangup",
                )?;
                call.state = MockCallState::Ended;
                let (corr, turns) = (call.correlation_id.clone(), call.turns);
                let ev = aokie_event(
                    "aokie.call.ended",
                    &corr,
                    json!({"at": now_iso8601(), "turns": turns, "reason": "operator_hangup"}),
                );
                emit_event(sink, &self.outbox, &ev, false).map_err(CmdError::failed)?;
                Ok(json!({"ended": true}))
            }
            "call.operatorSpeak" => {
                let obj = expect_fields(payload, &["text"])?;
                let text = require_str(&obj, "text")?;
                if text.trim().is_empty() {
                    return Err(CmdError::failed("text is empty"));
                }
                let call = self.require_call(&[MockCallState::Active], "call.operatorSpeak")?;
                call.turns += 1;
                // Mock: no audio path yet — acknowledge without faking
                // a TTS round-trip result.
                Ok(json!({"spoken": true, "mock": true}))
            }
            "sms.threads" => {
                expect_fields(payload, &[])?;
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
                let message_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
                let at = now_iso8601();
                // Essential event: outboxed before emission. The
                // correlation id is the SMS handle (contract §3).
                let ev = aokie_event(
                    "aokie.sms.sent",
                    &message_id,
                    json!({"messageId": message_id, "to": to, "at": at}),
                );
                emit_event(sink, &self.outbox, &ev, false).map_err(CmdError::failed)?;
                let thread = self.mock.thread_for(&to);
                thread.messages.push(MockSmsMessage {
                    id: message_id.clone(),
                    direction: "out",
                    body,
                    at,
                });
                Ok(json!({"messageId": message_id, "status": "queued"}))
            }
            "settings.get" => {
                let obj = expect_fields(payload, &["key"])?;
                match obj.get("key").and_then(Value::as_str) {
                    Some(key) => Ok(json!({
                        "key": key,
                        "value": self.store.config.settings.get(key).cloned(),
                    })),
                    None => Ok(json!({"settings": self.store.config.settings})),
                }
            }
            "settings.set" => {
                let obj = payload.as_object().ok_or_else(|| {
                    CmdError::failed("settings.set payload must be an object of key/value pairs")
                })?;
                if obj.is_empty() {
                    return Err(CmdError::failed("settings.set payload is empty"));
                }
                for (key, value) in obj {
                    self.store
                        .config
                        .settings
                        .insert(key.clone(), value.clone());
                }
                self.save_config()?;
                Ok(json!({"settings": self.store.config.settings}))
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
            emit_event(sink, &plugin.outbox, &ev, true).map_err(CmdError::failed)?;
            emitted.push(ev.name.clone());
            Ok::<(), CmdError>(())
        };

        emit(
            self,
            aokie_event(
                "aokie.dongle.detected",
                &corr,
                json!({"vid": dongle.vid, "pid": dongle.pid, "tier": dongle.tier, "source": "mock"}),
            ),
        )?;
        emit(
            self,
            aokie_event(
                "aokie.dongle.ready",
                &corr,
                json!({"vid": dongle.vid, "pid": dongle.pid}),
            ),
        )?;
        emit(
            self,
            aokie_event(
                "aokie.call.incoming",
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
            aokie_event("aokie.call.answered", &corr, json!({"at": now_iso8601()})),
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
                "aokie.call.ended",
                &corr,
                json!({"at": now_iso8601(), "durationMs": 42_000, "turns": 2}),
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
                "aokie.sms.received",
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
            Err(e) => Err(CmdError::failed(format!("WinUSB driver install failed: {e}"))),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn install_driver(&self, _payload: &Value) -> Result<Value, CmdError> {
        Err(CmdError::failed(
            "dongle.installDriver is only supported on Windows (WinUSB)",
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
                let known: std::collections::HashSet<(u16, u16)> = dongle_catalog::list_known_dongles()
                    .iter()
                    .map(|d| (d.vid, d.pid))
                    .collect();
                let out = devices
                    .into_iter()
                    .filter(|d| known.contains(&(d.vid, d.pid)) || d.driver.to_lowercase().contains("winusb"))
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
            Err(e) => (Vec::new(), Some(format!("live USB enumeration failed: {e}"))),
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn list_connected_dongles(&self) -> (Vec<Value>, Option<String>) {
        (Vec::new(), Some("live USB enumeration is Windows-only".to_string()))
    }

    fn outbox_counts(&self) -> Result<crate::outbox::OutboxCounts, CmdError> {
        self.outbox
            .counts()
            .map_err(|e| CmdError::failed(format!("outbox unavailable: {e}")))
    }
}

fn call_json(call: &MockCall) -> Value {
    json!({
        "correlationId": call.correlation_id,
        "caller": call.caller,
        "state": call.state.as_str(),
        "startedAt": call.started_at,
        "turns": call.turns,
    })
}

fn connector_error_line(id: &Value, err: &CmdError) -> String {
    rpc::error_line(
        Some(id),
        rpc::COMMAND_ERROR,
        &err.message,
        Some(json!({"code": err.code, "message": err.message})),
    )
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
        assert_eq!(parse(&resp)["result"]["status"], json!("ok"));

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
            .handle_rpc(connector_request(1, "dongle.levitate", json!(null)), &mut sink)
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
            .dispatch_command("dongle.diagnostics", &json!({"simulate": "call"}), &mut sink)
            .unwrap_err();
        assert!(err.message.contains("dev mode"));
        assert!(sink.lines.is_empty());
    }

    #[test]
    fn simulated_call_lifecycle_order_and_idempotency_keys() {
        let mut plugin = Plugin::ephemeral(true);
        let mut sink = VecSink::default();
        let data = plugin
            .dispatch_command("dongle.diagnostics", &json!({"simulate": "call"}), &mut sink)
            .unwrap();

        // Contract §4 order.
        let expected = [
            "aokie.dongle.detected",
            "aokie.dongle.ready",
            "aokie.call.incoming",
            "aokie.call.answered",
            "aokie.call.turn.final",
            "aokie.call.turn.final",
            "aokie.call.ended",
            "aokie.sms.received",
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
            .dispatch_command("dongle.diagnostics", &json!({"simulate": "call"}), &mut sink)
            .unwrap();
        // Scripted call already ended → answer fails typed.
        let err = plugin
            .dispatch_command("call.answer", &Value::Null, &mut sink)
            .unwrap_err();
        assert!(err.message.contains("ended"));

        // Rewind to incoming and drive it manually.
        plugin.mock.current_call.as_mut().unwrap().state = MockCallState::Incoming;
        sink.lines.clear();
        let data = plugin
            .dispatch_command("call.answer", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["answered"], json!(true));
        assert_eq!(data["call"]["state"], json!("active"));
        let v = parse(&sink.lines[0]);
        assert_eq!(v["params"]["event"]["name"], json!("aokie.call.answered"));

        let data = plugin
            .dispatch_command("call.current", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["call"]["state"], json!("active"));

        let data = plugin
            .dispatch_command("call.operatorSpeak", &json!({"text": "One moment"}), &mut sink)
            .unwrap();
        assert_eq!(data["spoken"], json!(true));

        sink.lines.clear();
        let data = plugin
            .dispatch_command("call.hangup", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["ended"], json!(true));
        let v = parse(&sink.lines[0]);
        assert_eq!(v["params"]["event"]["name"], json!("aokie.call.ended"));

        // Ended call: hangup again fails.
        assert!(plugin
            .dispatch_command("call.hangup", &Value::Null, &mut sink)
            .is_err());
    }

    #[test]
    fn sms_send_validates_and_emits_outboxed_event() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();

        let err = plugin
            .dispatch_command("sms.send", &json!({"to": "DROP TABLE", "body": "x"}), &mut sink)
            .unwrap_err();
        assert_eq!(err.code, "command_failed");
        let err = plugin
            .dispatch_command("sms.send", &json!({"to": "+61432123456", "body": ""}), &mut sink)
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
        let message_id = data["messageId"].as_str().unwrap();
        assert!(message_id.starts_with("sms_"));

        let v = parse(&sink.lines[0]);
        assert_eq!(v["params"]["event"]["name"], json!("aokie.sms.sent"));
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
        assert_eq!(reloaded.config.settings.get("mockCalls"), Some(&json!(true)));

        // Invalid payloads.
        assert!(plugin
            .dispatch_command("settings.set", &json!({}), &mut sink)
            .is_err());
        assert!(plugin
            .dispatch_command("settings.set", &json!(null), &mut sink)
            .is_err());
    }

    #[test]
    fn phone_commands_are_config_backed_mocks() {
        let mut plugin = Plugin::ephemeral(false);
        let mut sink = VecSink::default();

        let data = plugin
            .dispatch_command("phone.status", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["paired"], json!(false));

        let data = plugin
            .dispatch_command("phone.startPairing", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["status"], json!("pairing"));
        assert!(data["sessionId"].as_str().unwrap().starts_with("pair_"));
        let v = parse(&sink.lines[0]);
        assert_eq!(
            v["params"]["event"]["name"],
            json!("aokie.phone.pairing_started")
        );

        let data = plugin
            .dispatch_command("phone.stopPairing", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["stopped"], json!(true));

        let data = plugin
            .dispatch_command("phone.listPaired", &Value::Null, &mut sink)
            .unwrap();
        assert_eq!(data["devices"], json!([]));
    }
}
