//! MAP MCE (Message Access Profile — Client Equipment) driver.
//!
//! Phase 4a of the SMS auto-reply plan. Implements a synchronous state
//! machine on top of `obex` for three operations against a phone's MAS
//! (Message Access Server):
//!
//!   - `Operation::ListMessages` — GET `x-bt/MAP-msg-listing` to discover
//!     recent messages in a folder (default: `inbox`). The body is an
//!     XML document we hand off to the caller for parsing.
//!   - `Operation::GetMessage`  — GET `x-bt/message` for a single
//!     handle. The body is a bMessage envelope (text or binary).
//!   - `Operation::PushMessage` — PUT `x-bt/message` with a bMessage
//!     payload. Used to send an SMS reply via the phone.
//!
//! The state machine is **operation-scoped**: one `MasMceSession`
//! drives one operation end-to-end (CONNECT → SETPATH → GET/PUT →
//! DISCONNECT). The runtime constructs a fresh session per operation;
//! that keeps the state space tiny and avoids the "what if a phone
//! changes folders behind our back" footguns of a long-lived session.
//!
//! Spec references: MAP 1.4.2 §3.2 (Service Records), §5 (OBEX
//! operations), §3.3 (folder hierarchy), §3.5 (PushMessage).

#![allow(dead_code)] // Phase 4a — runtime integration follows in 4b.

use super::obex::{
    self, build_connect_request, build_disconnect, build_get_continuation, build_get_request,
    build_put_request, build_setpath, BodyAssembler, Header, Packet, HDR_CONNECTION_ID, HDR_SRM,
    RSP_CONTINUE, RSP_OK,
};

/// MAS service UUID (MAP §3.2.1). Big-endian on the wire. Spec value:
/// `bb582b40-420c-11db-b0de-0800200c9a66`.
pub const MAS_TARGET_UUID: [u8; 16] = [
    0xbb, 0x58, 0x2b, 0x40, 0x42, 0x0c, 0x11, 0xdb, 0xb0, 0xde, 0x08, 0x00, 0x20, 0x0c, 0x9a, 0x66,
];

/// Largest packet we'll accept inbound. Same as PBAP — 16 KB is what
/// every Android implementation we've seen advertises and is comfortably
/// above any single SMS body.
pub const DEFAULT_MAX_PACKET_LENGTH: u16 = 0x4000;

/// OBEX `Type:` strings that MAP uses verbatim (null terminator added by
/// the encoder). Pinning them as constants so a "let's tidy these up"
/// edit can't silently mis-spell what the phone matches against.
pub const TYPE_MSG_LISTING: &str = "x-bt/MAP-msg-listing";
pub const TYPE_MESSAGE: &str = "x-bt/message";
pub const TYPE_NOTIFICATION_REGISTRATION: &str = "x-bt/MAP-NotificationRegistration";

/// MAP application-parameter tag IDs we care about (MAP §6.3). Each tag
/// is a 1-byte ID, 1-byte length, then `length` bytes of value. We only
/// emit the handful needed for SMS auto-reply; the full set is large
/// and almost entirely about email/MMS that don't apply here.
pub const APP_PARAM_MAX_LIST_COUNT: u8 = 0x01; // 2 bytes BE
pub const APP_PARAM_LIST_START_OFFSET: u8 = 0x02; // 2 bytes BE
pub const APP_PARAM_FILTER_MESSAGE_TYPE: u8 = 0x03; // 1 byte bitmask
pub const APP_PARAM_TRANSPARENT: u8 = 0x0a; // 1 byte
pub const APP_PARAM_RETRY: u8 = 0x0b; // 1 byte
pub const APP_PARAM_CHARSET: u8 = 0x14; // 1 byte (0=Native, 1=UTF-8)
pub const APP_PARAM_NOTIFICATION_STATUS: u8 = 0x0e; // 1 byte (0=off, 1=on)

/// FilterMessageType bits (MAP §6.3.1). Set a bit to *exclude* that
/// type from the listing. We typically pass `0xfe` to keep only
/// SMS_GSM (bit 0 clear) — phones in the US typically advertise
/// SMS_CDMA (bit 1) instead, so the runtime picks the mask based on
/// what the AG declares it supports.
pub const MSG_TYPE_EXCLUDE_ALL_BUT_SMS_GSM: u8 = 0xfe; // keep bit 0
pub const MSG_TYPE_EXCLUDE_ALL_BUT_SMS_CDMA: u8 = 0xfd; // keep bit 1
pub const MSG_TYPE_KEEP_BOTH_SMS_VARIANTS: u8 = 0xfc; // keep bits 0+1

/// Charset values (MAP §6.3.5).
pub const CHARSET_NATIVE: u8 = 0x00;
pub const CHARSET_UTF8: u8 = 0x01;

/// Folders we know how to navigate to. Anything more exotic (drafts,
/// sent, etc.) can be added when a use case appears; SMS auto-reply
/// only needs inbox and outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Folder {
    Inbox,
    Outbox,
}

impl Folder {
    fn as_str(self) -> &'static str {
        match self {
            Folder::Inbox => "inbox",
            Folder::Outbox => "outbox",
        }
    }
}

/// What this session is being asked to do. Picked at construction
/// time; `next_request` and `handle_response` consult it to decide
/// what to send after CONNECT and folder navigation finish.
#[derive(Debug, Clone)]
pub enum Operation {
    /// Pull a message-listing XML from `folder`. `max_list_count` caps
    /// how many entries the AG returns (a value of 0 is special — it
    /// asks the AG to return only the count via NewMessage app-param,
    /// per MAP §5.4.5).
    ListMessages {
        folder: Folder,
        max_list_count: u16,
        filter_message_type: u8,
    },
    /// Fetch a single message body by its MAP handle (16 hex chars).
    /// The handle is what came back in the listing XML.
    GetMessage {
        folder: Folder,
        handle: String,
        charset: u8,
    },
    /// Push a single bMessage to `folder` (typically `outbox`). The
    /// AG forwards it to the carrier as an SMS.
    PushMessage {
        folder: Folder,
        bmessage: Vec<u8>,
        charset: u8,
    },
    /// Subscribe (or unsubscribe) for inbound notifications via MNS.
    /// Issues a PUT to `x-bt/MAP-NotificationRegistration` with body
    /// "0" (off) or "1" (on). The phone won't push EventReports to
    /// our MNS server until we've sent this with `enabled = true`
    /// at least once per session (MAP §3.7.1). No folder navigation —
    /// this PUT is at the OBEX root.
    SetNotificationRegistration { enabled: bool },
}

/// One step in a multi-SETPATH navigation chain. `Down(name)` is a
/// regular `cd <name>`; `Up` is a `cd ..` via the OBEX SETPATH BACKUP
/// flag. Pooled sessions need both — once the connection is alive, the
/// next op might want a sibling folder which means walking up first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetPathStep {
    Up,
    Down(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MasState {
    Idle,
    AwaitingConnect,
    /// Mid-navigation; `last` is the step we just emitted (so we can
    /// update `current_folder` on OK), `remaining` is the rest of the
    /// chain still to push.
    AwaitingSetPath {
        last: SetPathStep,
        remaining: Vec<SetPathStep>,
    },
    /// Initial GET sent (listing or single-message); awaiting first
    /// CONTINUE / OK.
    AwaitingFirstGet,
    /// GET continuation in progress (multi-packet body).
    AwaitingContinuation,
    /// PUT sent; awaiting OK.
    AwaitingPut,
    /// Phase 4g.d (pooling): operation finished cleanly and the
    /// caller asked the session to stay connected. Connection-Id is
    /// still valid; `current_folder` reflects where we are. Next
    /// transitions: `start_next_op(op)` → AwaitingSetPath/AwaitingPut/
    /// AwaitingFirstGet, or `request_disconnect()` → AwaitingDisconnect.
    Resting,
    /// DISCONNECT sent; awaiting final OK.
    AwaitingDisconnect,
    /// Terminal: operation complete. Caller can read the body or
    /// confirmation via the session accessors.
    Done,
    Failed(String),
}

/// What a finished session produced. Kept separate from `MasState` so
/// the caller can pattern-match on outcome without crossing
/// state-machine internals.
#[derive(Debug, Clone)]
pub enum OperationOutput {
    /// Raw XML body of the message-listing GET. The caller parses it;
    /// keeping XML out of this module avoids dragging an XML crate
    /// into protocol-layer code.
    Listing(Vec<u8>),
    /// Raw bMessage body of a single-message GET.
    Message(Vec<u8>),
    /// PUT was acknowledged by the AG.
    PushAcknowledged,
    /// No body yet — operation still in flight or failed before
    /// producing output.
    Pending,
}

#[derive(Debug)]
pub struct MasMceSession {
    state: MasState,
    operation: Operation,
    /// MAS instance-id we want to talk to (advertised by the AG in its
    /// SDP record). MAP supports multiple instances — typically one
    /// for SMS, one for email — so the runtime picks the right one
    /// before instantiating us.
    mas_instance_id: u8,
    connection_id: Option<u32>,
    body: BodyAssembler,
    output: OperationOutput,
    /// Phase 4g.d: when true, on PUT-OK / final GET-OK we transition
    /// to `Resting` instead of immediately sending DISCONNECT. Lets
    /// the orchestrator queue another op against the same OBEX/RFCOMM
    /// connection. Default false to preserve the legacy one-shot
    /// behaviour the existing tests assume.
    keep_alive: bool,
    /// Phase 4g.d: virtual current-working-directory inside the MAS
    /// filesystem. Updated when SETPATH succeeds. Empty == OBEX root.
    /// Used by `start_next_op` to compute the SETPATH chain (ups +
    /// downs) needed to reach the next op's target folder.
    current_folder: Vec<String>,
    /// Pixel/Bluedroid's MAS PSE flips Single Response Mode on for
    /// large GETs (message-listing, multi-packet bMessage) just like
    /// the PBAP PSE does for phonebook fetches. Without honoring SRM,
    /// our PCE-style continuations corrupt the streaming response and
    /// Pixel goes silent partway through, with no recovery — see
    /// `project_pbap_srm_pse` memory entry. We advertise SRM in the
    /// first GET, latch this flag when the PSE confirms with SRM=enable,
    /// and stop sending GET continuations while it's true. Reset per op
    /// in `start_next_op`.
    srm_active: bool,
}

impl MasMceSession {
    pub fn new(operation: Operation, mas_instance_id: u8) -> Self {
        Self {
            state: MasState::Idle,
            operation,
            mas_instance_id,
            connection_id: None,
            body: BodyAssembler::new(),
            output: OperationOutput::Pending,
            keep_alive: false,
            current_folder: Vec::new(),
            srm_active: false,
        }
    }

    pub fn state(&self) -> &MasState {
        &self.state
    }

    pub fn output(&self) -> &OperationOutput {
        &self.output
    }

    pub fn into_output(self) -> OperationOutput {
        self.output
    }

    /// Phase 4g.d: ask the session to stay connected after each op
    /// instead of auto-disconnecting. The orchestrator flips this on
    /// for the long-lived MAS session so back-to-back ops (e.g.
    /// fetch-message → push-reply for auto-reply) reuse the existing
    /// OBEX/RFCOMM/L2CAP connection.
    pub fn set_keep_alive(&mut self, keep_alive: bool) {
        self.keep_alive = keep_alive;
    }

    /// Phase 4g.d: snapshot of the current folder. Empty == OBEX root.
    /// Used by tests; the orchestrator uses `start_next_op` to drive
    /// navigation rather than reading this directly.
    pub fn current_folder(&self) -> &[String] {
        &self.current_folder
    }

    /// Produce the next outbound OBEX request. Only returns Some on
    /// the *first* call (which emits CONNECT); subsequent transitions
    /// are response-driven via `handle_response`.
    pub fn next_request(&mut self) -> Option<Vec<u8>> {
        match &self.state {
            MasState::Idle => {
                self.state = MasState::AwaitingConnect;
                Some(build_connect_request_with_mas_instance(
                    self.mas_instance_id,
                ))
            }
            _ => None,
        }
    }

    /// Feed the parsed response packet back in. Returns the next
    /// outbound packet to send, if any. Errors collapse cleanly into
    /// `MasState::Failed`.
    pub fn handle_response(&mut self, response: &Packet) -> Option<Vec<u8>> {
        match self.advance(response) {
            Ok(next) => next,
            Err(reason) => {
                self.state = MasState::Failed(reason);
                None
            }
        }
    }

    fn advance(&mut self, response: &Packet) -> Result<Option<Vec<u8>>, String> {
        match self.state.clone() {
            MasState::AwaitingConnect => {
                require_response_code(response, &[RSP_OK], "CONNECT")?;
                let conn = response
                    .header(HDR_CONNECTION_ID)
                    .and_then(|h| match h {
                        Header::Quad { value, .. } => Some(*value),
                        _ => None,
                    })
                    .ok_or_else(|| "MAS accepted CONNECT but omitted Connection-Id".to_string())?;
                self.connection_id = Some(conn);
                // OBEX CONNECT lands us at the AG's default working
                // directory which by MAP §3.3 is the root. Set
                // current_folder accordingly and let `navigate_to`
                // decide whether SETPATH steps are needed.
                self.current_folder.clear();
                let target = self.operation.target_folder();
                self.navigate_to(conn, &target)
            }
            MasState::AwaitingSetPath {
                last,
                mut remaining,
            } => {
                require_response_code(response, &[RSP_OK], "SETPATH")?;
                let conn = self.required_connection_id()?;
                // Apply the just-acknowledged step to current_folder.
                match last {
                    SetPathStep::Up => {
                        self.current_folder.pop();
                    }
                    SetPathStep::Down(name) => {
                        self.current_folder.push(name);
                    }
                }
                if remaining.is_empty() {
                    self.start_operation(conn)
                } else {
                    let next = remaining.remove(0);
                    let packet = build_setpath_step(conn, &next);
                    self.state = MasState::AwaitingSetPath {
                        last: next,
                        remaining,
                    };
                    Ok(Some(packet))
                }
            }
            MasState::AwaitingFirstGet | MasState::AwaitingContinuation => {
                let opcode = response.opcode;
                self.observe_srm(response);
                if opcode == RSP_CONTINUE {
                    self.body.push(response).map_err(|e| e.to_string())?;
                    let conn = self.required_connection_id()?;
                    self.state = MasState::AwaitingContinuation;
                    if self.srm_active {
                        // Server is streaming under SRM — wait for the
                        // next chunk without sending a continuation.
                        // Sending one corrupts the PSE and Pixel goes
                        // silent partway through the response.
                        Ok(None)
                    } else {
                        Ok(Some(build_get_continuation(conn)))
                    }
                } else if opcode == RSP_OK {
                    self.body.push(response).map_err(|e| e.to_string())?;
                    if !self.body.is_complete() {
                        return Err("MAS server sent OK without EndOfBody".to_string());
                    }
                    self.output = match &self.operation {
                        Operation::ListMessages { .. } => {
                            OperationOutput::Listing(self.body.bytes().to_vec())
                        }
                        Operation::GetMessage { .. } => {
                            OperationOutput::Message(self.body.bytes().to_vec())
                        }
                        Operation::PushMessage { .. }
                        | Operation::SetNotificationRegistration { .. } => {
                            // Unreachable: both ops route through
                            // AwaitingPut, not AwaitingFirstGet.
                            return Err(
                                "MAS GET response arrived for a PUT-shaped operation".into()
                            );
                        }
                    };
                    self.finalize_op_or_disconnect()
                } else {
                    Err(format!("MAS GET: unexpected response 0x{:02x}", opcode))
                }
            }
            MasState::AwaitingPut => {
                require_response_code(response, &[RSP_OK], "PUT")?;
                self.output = OperationOutput::PushAcknowledged;
                self.finalize_op_or_disconnect()
            }
            MasState::AwaitingDisconnect => {
                if response.opcode != RSP_OK {
                    eprintln!(
                        "[MAP] DISCONNECT returned 0x{:02x} — proceeding as Done anyway",
                        response.opcode
                    );
                }
                self.state = MasState::Done;
                Ok(None)
            }
            MasState::Resting => {
                Err("MAS response received while Resting (no op in flight)".to_string())
            }
            MasState::Idle => Err("MAS response received while Idle".to_string()),
            MasState::Done | MasState::Failed(_) => {
                Err("MAS response received after session terminated".to_string())
            }
        }
    }

    /// Phase 4g.d: at op-completion, either park in `Resting` (pooling
    /// is on, orchestrator may queue another op) or send DISCONNECT.
    /// This is the single seam where keep_alive influences behaviour;
    /// every legacy code path that called `MasState::AwaitingDisconnect`
    /// inline now routes through here.
    fn finalize_op_or_disconnect(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.keep_alive {
            self.state = MasState::Resting;
            Ok(None)
        } else {
            let conn = self.required_connection_id()?;
            self.state = MasState::AwaitingDisconnect;
            Ok(Some(build_disconnect(Some(conn))))
        }
    }

    /// Compute the SETPATH chain from `current_folder` to `target` and
    /// kick it off. Walks UP via SETPATH-BACKUP for each level above the
    /// common prefix, then DOWN for each level below it. If the target
    /// matches the current folder exactly, jumps straight to the
    /// op-specific request (no SETPATH at all).
    fn navigate_to(
        &mut self,
        conn: u32,
        target: &[&'static str],
    ) -> Result<Option<Vec<u8>>, String> {
        let common = self
            .current_folder
            .iter()
            .zip(target.iter())
            .take_while(|(a, b)| a.as_str() == **b)
            .count();
        let ups = self.current_folder.len() - common;
        let downs: Vec<&'static str> = target.iter().skip(common).copied().collect();
        let mut steps: Vec<SetPathStep> = std::iter::repeat(SetPathStep::Up).take(ups).collect();
        steps.extend(downs.into_iter().map(|s| SetPathStep::Down(s.to_string())));
        if steps.is_empty() {
            // Already at the target folder — typical for the second
            // op in a row that targets the same folder. Skip SETPATH.
            return self.start_operation(conn);
        }
        let mut remaining = steps;
        let first = remaining.remove(0);
        let packet = build_setpath_step(conn, &first);
        self.state = MasState::AwaitingSetPath {
            last: first,
            remaining,
        };
        Ok(Some(packet))
    }

    /// Phase 4g.d: assign a new operation against the existing
    /// connection. Caller must verify state == Resting; otherwise this
    /// returns Err. Re-uses connection_id and current_folder; emits
    /// either a SETPATH chain to the new folder or, if the op is
    /// already at the right folder, the op-specific request directly.
    pub fn start_next_op(&mut self, operation: Operation) -> Result<Vec<u8>, String> {
        if !matches!(self.state, MasState::Resting) {
            return Err(format!(
                "start_next_op called in non-Resting state: {:?}",
                self.state
            ));
        }
        let conn = self.required_connection_id()?;
        // Reset per-op state. body / output are produced fresh by
        // the upcoming op; operation itself flips to the new one.
        // SRM is also a per-op latch — Pixel re-asserts it on the first
        // GET response of every operation, so the previous op's flag
        // must NOT carry over (otherwise the next GET silently skips
        // its required continuation).
        self.body = BodyAssembler::new();
        self.output = OperationOutput::Pending;
        self.operation = operation;
        self.srm_active = false;
        let target = self.operation.target_folder();
        let next = self.navigate_to(conn, &target)?;
        next.ok_or_else(|| {
            // navigate_to always emits at least one packet; an empty
            // result means current_folder already matched and the
            // op-specific request was emitted instead — but
            // start_operation always returns Some(_) for our four
            // op variants, so None here is genuinely unreachable.
            "start_next_op produced no outbound packet (state machine bug)".to_string()
        })
    }

    /// Phase 4g.d: tear down the pooled session. Caller must verify
    /// state == Resting. Sends OBEX DISCONNECT; on response the state
    /// goes to Done.
    pub fn request_disconnect(&mut self) -> Result<Vec<u8>, String> {
        if !matches!(self.state, MasState::Resting) {
            return Err(format!(
                "request_disconnect called in non-Resting state: {:?}",
                self.state
            ));
        }
        let conn = self.required_connection_id()?;
        self.state = MasState::AwaitingDisconnect;
        Ok(build_disconnect(Some(conn)))
    }

    /// All folder hops done; emit the operation-specific OBEX request.
    fn start_operation(&mut self, conn: u32) -> Result<Option<Vec<u8>>, String> {
        match self.operation.clone() {
            Operation::ListMessages {
                max_list_count,
                filter_message_type,
                ..
            } => {
                let mut app_params = Vec::with_capacity(8);
                push_app_param_u16(&mut app_params, APP_PARAM_MAX_LIST_COUNT, max_list_count);
                push_app_param_u8(
                    &mut app_params,
                    APP_PARAM_FILTER_MESSAGE_TYPE,
                    filter_message_type,
                );
                let extras = vec![
                    Header::obex_type(TYPE_MSG_LISTING),
                    Header::app_parameters(app_params),
                    // Advertise SRM support. Pixel's MAS PSE is observed
                    // to flip on Single Response Mode for multi-packet
                    // listings; if it does, `observe_srm` will latch
                    // `srm_active` and we'll skip GET continuations.
                    Header::Byte {
                        id: HDR_SRM,
                        value: 0x01,
                    },
                ];
                self.state = MasState::AwaitingFirstGet;
                Ok(Some(build_get_request(conn, extras)))
            }
            Operation::GetMessage {
                handle, charset, ..
            } => {
                let mut app_params = Vec::with_capacity(3);
                push_app_param_u8(&mut app_params, APP_PARAM_CHARSET, charset);
                let extras = vec![
                    Header::name(handle),
                    Header::obex_type(TYPE_MESSAGE),
                    Header::app_parameters(app_params),
                    // Same SRM=enable handshake as ListMessages — large
                    // bMessage bodies (MMS / long SMS chains) span many
                    // OBEX packets and the PSE may flip SRM for them.
                    Header::Byte {
                        id: HDR_SRM,
                        value: 0x01,
                    },
                ];
                self.state = MasState::AwaitingFirstGet;
                Ok(Some(build_get_request(conn, extras)))
            }
            Operation::PushMessage {
                bmessage, charset, ..
            } => {
                let mut app_params = Vec::with_capacity(6);
                push_app_param_u8(&mut app_params, APP_PARAM_CHARSET, charset);
                push_app_param_u8(&mut app_params, APP_PARAM_TRANSPARENT, 0);
                push_app_param_u8(&mut app_params, APP_PARAM_RETRY, 1);
                let extras = vec![
                    // Empty Name selects the current folder (outbox)
                    // per MAP §5.4.7. Some phones reject an absent
                    // Name header so we always include it.
                    Header::name(""),
                    Header::obex_type(TYPE_MESSAGE),
                    Header::app_parameters(app_params),
                ];
                self.state = MasState::AwaitingPut;
                Ok(Some(build_put_request(conn, extras, bmessage)))
            }
            Operation::SetNotificationRegistration { enabled } => {
                // Body is one ASCII byte: "1" or "0" per MAP §6.3.5.
                // Some phones tolerate either ascii or the binary u8
                // form; ascii is what every reference implementation
                // emits, so use that.
                let body = if enabled {
                    b"1".to_vec()
                } else {
                    b"0".to_vec()
                };
                // App-param NotificationStatus mirrors the body — most
                // phones look at the app-param, not the body, but
                // both are mandatory per spec.
                let mut app_params = Vec::with_capacity(3);
                push_app_param_u8(
                    &mut app_params,
                    APP_PARAM_NOTIFICATION_STATUS,
                    if enabled { 1 } else { 0 },
                );
                let extras = vec![
                    Header::name(""),
                    Header::obex_type(TYPE_NOTIFICATION_REGISTRATION),
                    Header::app_parameters(app_params),
                ];
                self.state = MasState::AwaitingPut;
                Ok(Some(build_put_request(conn, extras, body)))
            }
        }
    }

    fn required_connection_id(&self) -> Result<u32, String> {
        self.connection_id
            .ok_or_else(|| "MAS session has no Connection-Id".to_string())
    }

    /// Watch a GET-phase response for the SRM=enable header. The PSE may
    /// add it to any reply during the GET phase; once seen we latch it
    /// on for the rest of THIS operation. Reset back to false in
    /// `start_next_op` so a subsequent op gets to negotiate fresh.
    /// Mirrors `pbap::PbapPceSession::observe_srm` deliberately —
    /// keeping the two SRM-aware sessions structurally similar means
    /// the next time a third PSE-style profile shows up we can copy
    /// the same shape again.
    fn observe_srm(&mut self, response: &Packet) {
        if self.srm_active {
            return;
        }
        if let Some(Header::Byte { value, .. }) = response.header(obex::HDR_SRM) {
            if *value == 0x01 {
                self.srm_active = true;
                println!(
                    "[AokieRadio] MAS SRM=enable confirmed by PSE — pausing GET continuations \
                     for this op"
                );
            }
        }
    }
}

impl Operation {
    fn folder(&self) -> Option<Folder> {
        match self {
            Operation::ListMessages { folder, .. }
            | Operation::GetMessage { folder, .. }
            | Operation::PushMessage { folder, .. } => Some(*folder),
            Operation::SetNotificationRegistration { .. } => None,
        }
    }

    /// Path components from OBEX root that the op needs to be in
    /// before emitting its OBEX request. SetNotificationRegistration
    /// runs at root so it's empty; the rest sit under
    /// `/telecom/msg/<folder>`. Static strs avoid the allocation
    /// churn of a Vec<String> for what's structurally a constant.
    fn target_folder(&self) -> Vec<&'static str> {
        match self.folder() {
            Some(folder) => vec!["telecom", "msg", folder.as_str()],
            None => Vec::new(),
        }
    }
}

/// Build the OBEX SETPATH packet for a single navigation step, using
/// the BACKUP flag for `Up` and a normal name-carrying SETPATH for
/// `Down`. NOCREATE is set on Up to keep it a pure cd-up; the AG
/// must not create a new folder when we're walking out.
fn build_setpath_step(connection_id: u32, step: &SetPathStep) -> Vec<u8> {
    match step {
        SetPathStep::Up => build_setpath(
            connection_id,
            None,
            obex::SETPATH_FLAG_BACKUP | obex::SETPATH_FLAG_NOCREATE,
        ),
        SetPathStep::Down(name) => build_setpath(connection_id, Some(name), 0),
    }
}

/// Build the MAS CONNECT with the standard MAP-required app-params
/// block (instance-id). MAS instance-id is mandatory per MAP §5.1 and
/// the AG will refuse CONNECT without it on multi-instance phones.
fn build_connect_request_with_mas_instance(mas_instance_id: u8) -> Vec<u8> {
    // We extend the generic CONNECT helper by tacking AppParams into
    // the encoded packet. Cleaner long-term to push this into obex.rs,
    // but doing so would mean changing every existing CONNECT caller —
    // not worth it for one extra header. Re-encode at the Packet
    // level instead.
    let bytes = build_connect_request(Some(&MAS_TARGET_UUID), DEFAULT_MAX_PACKET_LENGTH);
    let mut packet = Packet::parse(&bytes, obex::FixedPayload::Connect)
        .expect("self-emitted CONNECT must round-trip");
    let mut ap = Vec::with_capacity(3);
    push_app_param_u8(&mut ap, 0x0F /* MASInstanceID */, mas_instance_id);
    packet.headers.push(Header::app_parameters(ap));
    packet.encode()
}

fn require_response_code(packet: &Packet, expected: &[u8], context: &str) -> Result<(), String> {
    if expected.contains(&packet.opcode) {
        Ok(())
    } else {
        Err(format!(
            "MAS {}: server returned 0x{:02x} (expected one of {:?})",
            context, packet.opcode, expected
        ))
    }
}

fn push_app_param_u8(out: &mut Vec<u8>, tag: u8, value: u8) {
    out.push(tag);
    out.push(1);
    out.push(value);
}

fn push_app_param_u16(out: &mut Vec<u8>, tag: u8, value: u16) {
    out.push(tag);
    out.push(2);
    out.extend_from_slice(&value.to_be_bytes());
}

// =============================================================================
// Tests.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aokie_radio::obex::{
        build_connect_response, FixedPayload, HDR_APP_PARAMETERS, HDR_NAME, HDR_TARGET, HDR_TYPE,
        OP_CONNECT, OP_DISCONNECT, OP_GET_FINAL, OP_PUT_FINAL, OP_SETPATH,
    };

    fn parse_outbound(bytes: &[u8], fixed: FixedPayload) -> Packet {
        Packet::parse(bytes, fixed).expect("outbound packet must round-trip")
    }

    fn ok_response(connection_id: u32, headers: Vec<Header>) -> Packet {
        let mut h = vec![Header::connection_id(connection_id)];
        h.extend(headers);
        Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: h,
        }
    }

    fn ok_connect_response(connection_id: u32) -> Packet {
        let bytes = build_connect_response(
            RSP_OK,
            DEFAULT_MAX_PACKET_LENGTH,
            Some(connection_id),
            Some(MAS_TARGET_UUID.to_vec()),
        );
        Packet::parse(&bytes, FixedPayload::Connect).expect("CONNECT response must round-trip")
    }

    #[test]
    fn mas_target_uuid_matches_spec() {
        // Pin the MAS target UUID so a future "tidy these up" change
        // can't silently change what we negotiate against. Bytes are
        // taken from MAP 1.4.2 §3.2.1.
        assert_eq!(
            MAS_TARGET_UUID,
            [
                0xbb, 0x58, 0x2b, 0x40, 0x42, 0x0c, 0x11, 0xdb, 0xb0, 0xde, 0x08, 0x00, 0x20, 0x0c,
                0x9a, 0x66
            ]
        );
    }

    #[test]
    fn first_request_is_connect_with_mas_target_and_instance_id() {
        let mut session = MasMceSession::new(
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 16,
                filter_message_type: MSG_TYPE_KEEP_BOTH_SMS_VARIANTS,
            },
            0, // instance 0 = SMS on most phones
        );
        let bytes = session.next_request().expect("first request");
        let parsed = parse_outbound(&bytes, FixedPayload::Connect);
        assert_eq!(parsed.opcode, OP_CONNECT);

        let target = parsed.header(HDR_TARGET).expect("Target header required");
        match target {
            Header::ByteSeq { value, .. } => assert_eq!(value, &MAS_TARGET_UUID),
            _ => panic!("Target must be byte-seq"),
        }

        // The MAS instance-id must be present as an app-parameter on
        // CONNECT. Tag 0x0F, length 1, value 0.
        let ap = parsed
            .header(HDR_APP_PARAMETERS)
            .expect("AppParameters required on MAS CONNECT");
        match ap {
            Header::ByteSeq { value, .. } => {
                assert_eq!(&value[..], &[0x0F, 0x01, 0x00]);
            }
            _ => panic!("AppParameters must be byte-seq"),
        }
    }

    #[test]
    fn list_messages_happy_path_through_disconnect() {
        let mut session = MasMceSession::new(
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 16,
                filter_message_type: MSG_TYPE_KEEP_BOTH_SMS_VARIANTS,
            },
            0,
        );
        let _connect = session.next_request().unwrap();

        // CONNECT OK → expect SETPATH telecom.
        let next = session
            .handle_response(&ok_connect_response(0x1234))
            .expect("after CONNECT we send SETPATH telecom");
        let parsed = parse_outbound(&next, FixedPayload::SetPath);
        assert_eq!(parsed.opcode, OP_SETPATH);
        match parsed.header(HDR_NAME).unwrap() {
            Header::Unicode { value, .. } => assert_eq!(value, "telecom"),
            _ => panic!("Name must be Unicode"),
        }

        // SETPATH telecom OK → expect SETPATH msg.
        let next = session
            .handle_response(&ok_response(0x1234, vec![]))
            .expect("SETPATH msg");
        let parsed = parse_outbound(&next, FixedPayload::SetPath);
        match parsed.header(HDR_NAME).unwrap() {
            Header::Unicode { value, .. } => assert_eq!(value, "msg"),
            _ => panic!("Name must be Unicode"),
        }

        // SETPATH msg OK → expect SETPATH inbox.
        let next = session
            .handle_response(&ok_response(0x1234, vec![]))
            .expect("SETPATH inbox");
        let parsed = parse_outbound(&next, FixedPayload::SetPath);
        match parsed.header(HDR_NAME).unwrap() {
            Header::Unicode { value, .. } => assert_eq!(value, "inbox"),
            _ => panic!("Name must be Unicode"),
        }

        // SETPATH inbox OK → expect GET msg-listing.
        let next = session
            .handle_response(&ok_response(0x1234, vec![]))
            .expect("GET msg-listing");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_GET_FINAL);
        match parsed.header(HDR_TYPE).unwrap() {
            Header::ByteSeq { value, .. } => assert_eq!(value, b"x-bt/MAP-msg-listing\0"),
            _ => panic!("Type must be byte-seq"),
        }
        // App-params must include MaxListCount=16 and FilterMessageType.
        let ap = parsed.header(HDR_APP_PARAMETERS).unwrap();
        match ap {
            Header::ByteSeq { value, .. } => {
                assert!(value.windows(4).any(|w| w == [0x01, 0x02, 0x00, 0x10])); // MaxListCount=16
                assert!(value.windows(3).any(|w| w
                    == [
                        APP_PARAM_FILTER_MESSAGE_TYPE,
                        0x01,
                        MSG_TYPE_KEEP_BOTH_SMS_VARIANTS
                    ]));
            }
            _ => panic!("AppParameters must be byte-seq"),
        }

        // PSE replies OK + EndOfBody with the listing XML → expect DISCONNECT.
        let listing = b"<MAP-msg-listing version=\"1.0\"></MAP-msg-listing>".to_vec();
        let final_ok = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0x1234),
                Header::end_of_body(listing.clone()),
            ],
        };
        let next = session.handle_response(&final_ok).expect("DISCONNECT");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_DISCONNECT);
        match session.output() {
            OperationOutput::Listing(bytes) => assert_eq!(bytes, &listing),
            other => panic!("expected Listing output, got {:?}", other),
        }

        // DISCONNECT OK → terminal Done.
        let _ = session.handle_response(&Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![],
        });
        assert_eq!(*session.state(), MasState::Done);
    }

    #[test]
    fn get_message_uses_handle_as_name_header() {
        let mut session = MasMceSession::new(
            Operation::GetMessage {
                folder: Folder::Inbox,
                handle: "0123456789ABCDEF".to_string(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![])); // telecom
        let _ = session.handle_response(&ok_response(7, vec![])); // msg
        let next = session
            .handle_response(&ok_response(7, vec![]))
            .expect("GET message");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_GET_FINAL);
        match parsed.header(HDR_NAME).unwrap() {
            Header::Unicode { value, .. } => assert_eq!(value, "0123456789ABCDEF"),
            _ => panic!("Name must be Unicode"),
        }
        match parsed.header(HDR_TYPE).unwrap() {
            Header::ByteSeq { value, .. } => assert_eq!(value, b"x-bt/message\0"),
            _ => panic!("Type must be byte-seq"),
        }
    }

    #[test]
    fn push_message_emits_put_with_bmessage_in_endofbody() {
        let bmessage = b"BEGIN:BMSG\r\nVERSION:1.0\r\nEND:BMSG\r\n".to_vec();
        let mut session = MasMceSession::new(
            Operation::PushMessage {
                folder: Folder::Outbox,
                bmessage: bmessage.clone(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![])); // telecom
        let _ = session.handle_response(&ok_response(7, vec![])); // msg
        let next = session
            .handle_response(&ok_response(7, vec![])) // outbox
            .expect("PUT message");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_PUT_FINAL);
        // EndOfBody should carry our bmessage bytes verbatim.
        let eob = parsed
            .headers
            .iter()
            .find(|h| matches!(h, Header::ByteSeq { id, .. } if *id == obex::HDR_END_OF_BODY))
            .expect("PUT must carry EndOfBody");
        match eob {
            Header::ByteSeq { value, .. } => assert_eq!(value, &bmessage),
            _ => unreachable!(),
        }

        // PUT OK → DISCONNECT goes out and output flips to acknowledged.
        let next = session
            .handle_response(&ok_response(7, vec![]))
            .expect("DISCONNECT");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_DISCONNECT);
        assert!(matches!(
            session.output(),
            OperationOutput::PushAcknowledged
        ));
    }

    #[test]
    fn connect_failure_collapses_to_failed() {
        let mut session = MasMceSession::new(
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 1,
                filter_message_type: 0,
            },
            0,
        );
        let _ = session.next_request();
        let denied = Packet {
            opcode: obex::RSP_FORBIDDEN,
            fixed_payload: vec![0x10, 0x00, 0x40, 0x00],
            headers: vec![],
        };
        let next = session.handle_response(&denied);
        assert!(next.is_none());
        assert!(matches!(session.state(), MasState::Failed(_)));
    }

    #[test]
    fn put_failure_collapses_to_failed_without_disconnect() {
        let mut session = MasMceSession::new(
            Operation::PushMessage {
                folder: Folder::Outbox,
                bmessage: b"BEGIN:BMSG\r\nEND:BMSG\r\n".to_vec(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![]));
        let _ = session.handle_response(&ok_response(7, vec![]));
        let _ = session.handle_response(&ok_response(7, vec![])); // outbox
                                                                  // PUT comes back with NOT_ACCEPTABLE — phone refused the SMS.
        let put_failed = Packet {
            opcode: obex::RSP_NOT_ACCEPTABLE,
            fixed_payload: vec![],
            headers: vec![],
        };
        let next = session.handle_response(&put_failed);
        assert!(next.is_none());
        assert!(matches!(session.state(), MasState::Failed(_)));
    }

    #[test]
    fn set_notification_registration_skips_folder_navigation_and_emits_put_at_root() {
        let mut session =
            MasMceSession::new(Operation::SetNotificationRegistration { enabled: true }, 0);
        let _ = session.next_request();
        // CONNECT OK should be followed *directly* by the PUT — no
        // SETPATH telecom/msg.
        let next = session
            .handle_response(&ok_connect_response(7))
            .expect("after CONNECT we send PUT directly");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_PUT_FINAL);
        match parsed.header(HDR_TYPE).unwrap() {
            Header::ByteSeq { value, .. } => {
                assert_eq!(value, b"x-bt/MAP-NotificationRegistration\0");
            }
            _ => panic!("Type must be byte-seq"),
        }
        // EndOfBody body is "1" (enabled) per MAP §6.3.5.
        let eob = parsed
            .headers
            .iter()
            .find(|h| matches!(h, Header::ByteSeq { id, .. } if *id == obex::HDR_END_OF_BODY))
            .expect("PUT must carry EndOfBody");
        match eob {
            Header::ByteSeq { value, .. } => assert_eq!(value, b"1"),
            _ => unreachable!(),
        }
        // App-param NotificationStatus tag 0x0e, length 1, value 1.
        let ap = parsed.header(HDR_APP_PARAMETERS).unwrap();
        match ap {
            Header::ByteSeq { value, .. } => {
                assert!(value
                    .windows(3)
                    .any(|w| w == [APP_PARAM_NOTIFICATION_STATUS, 0x01, 0x01]));
            }
            _ => panic!("AppParameters must be byte-seq"),
        }

        // PUT OK → DISCONNECT, output flips to PushAcknowledged.
        let next = session
            .handle_response(&ok_response(7, vec![]))
            .expect("DISCONNECT");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_DISCONNECT);
        assert!(matches!(
            session.output(),
            OperationOutput::PushAcknowledged
        ));
    }

    #[test]
    fn next_request_after_terminal_returns_none() {
        let mut session = MasMceSession::new(
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 0,
                filter_message_type: 0,
            },
            0,
        );
        session.state = MasState::Done;
        assert!(session.next_request().is_none());
        session.state = MasState::Failed("test".into());
        assert!(session.next_request().is_none());
    }

    #[test]
    fn response_after_done_returns_failed() {
        let mut session = MasMceSession::new(
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 0,
                filter_message_type: 0,
            },
            0,
        );
        session.state = MasState::Done;
        let _ = session.handle_response(&Packet {
            opcode: RSP_OK,
            fixed_payload: vec![],
            headers: vec![],
        });
        assert!(matches!(session.state(), MasState::Failed(_)));
    }

    // -------------------------------------------------------------
    // Phase 4g.d: pooled (keep_alive) session tests.
    // -------------------------------------------------------------

    /// Drive a session through CONNECT + SETPATH×3 + PUT-OK with
    /// `keep_alive=true`. Expect the session to land in `Resting`
    /// (no DISCONNECT yet) with `current_folder = telecom/msg/outbox`.
    fn run_push_into_resting(session: &mut MasMceSession, conn_id: u32) {
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(conn_id));
        let _ = session.handle_response(&ok_response(conn_id, vec![])); // telecom
        let _ = session.handle_response(&ok_response(conn_id, vec![])); // msg
        let _ = session.handle_response(&ok_response(conn_id, vec![])); // outbox
                                                                        // PUT was emitted on SETPATH-outbox-OK; now send PUT-OK.
        let next = session.handle_response(&ok_response(conn_id, vec![]));
        // With keep_alive, finalize_op_or_disconnect returns None
        // (the session parks in Resting; no DISCONNECT emitted).
        assert!(next.is_none());
    }

    #[test]
    fn keep_alive_session_parks_in_resting_after_put_ok() {
        let mut session = MasMceSession::new(
            Operation::PushMessage {
                folder: Folder::Outbox,
                bmessage: b"BEGIN:BMSG\r\nEND:BMSG\r\n".to_vec(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        session.set_keep_alive(true);
        run_push_into_resting(&mut session, 7);
        assert_eq!(*session.state(), MasState::Resting);
        assert_eq!(
            session.current_folder(),
            &[
                "telecom".to_string(),
                "msg".to_string(),
                "outbox".to_string()
            ]
        );
        assert!(matches!(
            session.output(),
            OperationOutput::PushAcknowledged
        ));
    }

    #[test]
    fn start_next_op_to_sibling_folder_emits_one_up_then_one_down() {
        // Auto-reply shape: GET inbox/<msg> finishes, runtime then
        // queues PushMessage to outbox. From /telecom/msg/inbox to
        // /telecom/msg/outbox is exactly 1 Up + 1 Down — no full
        // re-navigation. This is the whole point of pooling.
        let mut session = MasMceSession::new(
            Operation::GetMessage {
                folder: Folder::Inbox,
                handle: "0123456789ABCDEF".to_string(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        session.set_keep_alive(true);
        // CONNECT + walk to inbox.
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(11));
        let _ = session.handle_response(&ok_response(11, vec![])); // telecom
        let _ = session.handle_response(&ok_response(11, vec![])); // msg
        let _ = session.handle_response(&ok_response(11, vec![])); // inbox
                                                                   // GET emitted; final OK with EndOfBody body.
        let body_packet = Packet {
            opcode: RSP_OK,
            fixed_payload: vec![],
            headers: vec![Header::end_of_body(b"<MAP-msg-listing/>".to_vec())],
        };
        let next = session.handle_response(&body_packet);
        assert!(
            next.is_none(),
            "keep_alive session must park in Resting, not DISCONNECT"
        );
        assert_eq!(*session.state(), MasState::Resting);
        assert_eq!(
            session.current_folder(),
            &[
                "telecom".to_string(),
                "msg".to_string(),
                "inbox".to_string()
            ]
        );

        // Hand it the next op (push to outbox) — expect a SETPATH-up
        // (no Name, BACKUP flag) as the first emitted packet.
        let first = session
            .start_next_op(Operation::PushMessage {
                folder: Folder::Outbox,
                bmessage: b"BEGIN:BMSG\r\nEND:BMSG\r\n".to_vec(),
                charset: CHARSET_UTF8,
            })
            .expect("start_next_op produces a packet");
        let parsed = parse_outbound(&first, FixedPayload::SetPath);
        assert_eq!(parsed.opcode, OP_SETPATH);
        // Up step: no Name header, BACKUP|NOCREATE flags.
        assert!(parsed.header(HDR_NAME).is_none(), "Up step has no Name");
        assert_eq!(
            parsed.fixed_payload[0],
            obex::SETPATH_FLAG_BACKUP | obex::SETPATH_FLAG_NOCREATE,
        );

        // SETPATH-up OK → expect SETPATH-down("outbox").
        let next = session
            .handle_response(&ok_response(11, vec![]))
            .expect("SETPATH outbox");
        let parsed = parse_outbound(&next, FixedPayload::SetPath);
        match parsed.header(HDR_NAME).unwrap() {
            Header::Unicode { value, .. } => assert_eq!(value, "outbox"),
            _ => panic!("Name must be Unicode"),
        }

        // SETPATH-outbox OK → expect PUT (no further nav).
        let next = session
            .handle_response(&ok_response(11, vec![]))
            .expect("PUT");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_PUT_FINAL);
        assert_eq!(*session.state(), MasState::AwaitingPut);
        assert_eq!(
            session.current_folder(),
            &[
                "telecom".to_string(),
                "msg".to_string(),
                "outbox".to_string()
            ]
        );
    }

    #[test]
    fn start_next_op_to_same_folder_skips_setpath_entirely() {
        // Two GetMessages from the same folder back-to-back: the
        // second op should skip SETPATH and emit GET directly.
        let mut session = MasMceSession::new(
            Operation::GetMessage {
                folder: Folder::Inbox,
                handle: "AAAA".to_string(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        session.set_keep_alive(true);
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![])); // telecom
        let _ = session.handle_response(&ok_response(7, vec![])); // msg
        let _ = session.handle_response(&ok_response(7, vec![])); // inbox
                                                                  // GET → final OK
        let _ = session.handle_response(&Packet {
            opcode: RSP_OK,
            fixed_payload: vec![],
            headers: vec![Header::end_of_body(b"first body".to_vec())],
        });
        assert_eq!(*session.state(), MasState::Resting);

        // Same folder, different handle — expect direct GET.
        let next = session
            .start_next_op(Operation::GetMessage {
                folder: Folder::Inbox,
                handle: "BBBB".to_string(),
                charset: CHARSET_UTF8,
            })
            .expect("start_next_op packet");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_GET_FINAL);
        match parsed.header(HDR_NAME).unwrap() {
            Header::Unicode { value, .. } => assert_eq!(value, "BBBB"),
            _ => panic!("Name must be Unicode"),
        }
        assert_eq!(*session.state(), MasState::AwaitingFirstGet);
    }

    #[test]
    fn start_next_op_from_folder_to_root_emits_three_ups() {
        // After GetMessage from inbox, queue a SetNotificationRegistration
        // (which lives at root). Expect 3 SETPATH-up steps.
        let mut session = MasMceSession::new(
            Operation::GetMessage {
                folder: Folder::Inbox,
                handle: "AA".to_string(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        session.set_keep_alive(true);
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![])); // telecom
        let _ = session.handle_response(&ok_response(7, vec![])); // msg
        let _ = session.handle_response(&ok_response(7, vec![])); // inbox
        let _ = session.handle_response(&Packet {
            opcode: RSP_OK,
            fixed_payload: vec![],
            headers: vec![Header::end_of_body(b"x".to_vec())],
        });
        assert_eq!(*session.state(), MasState::Resting);

        let first = session
            .start_next_op(Operation::SetNotificationRegistration { enabled: true })
            .expect("first packet");
        let parsed = parse_outbound(&first, FixedPayload::SetPath);
        assert!(parsed.header(HDR_NAME).is_none());
        assert_eq!(
            parsed.fixed_payload[0] & obex::SETPATH_FLAG_BACKUP,
            obex::SETPATH_FLAG_BACKUP
        );

        // 3 ups in total — count by walking SETPATHs until something
        // else comes out (the PUT). Peek at the raw opcode byte
        // before deciding which FixedPayload shape to use; SETPATH
        // has 2 fixed bytes, PUT has 0, and parsing one as the other
        // misinterprets the payload.
        let mut ups_after_first = 0;
        loop {
            let next = session
                .handle_response(&ok_response(7, vec![]))
                .expect("response packet");
            if next[0] == OP_SETPATH {
                ups_after_first += 1;
            } else {
                let put = parse_outbound(&next, FixedPayload::None);
                assert_eq!(put.opcode, OP_PUT_FINAL);
                break;
            }
        }
        assert_eq!(
            ups_after_first, 2,
            "expected 2 more ups after the first emit"
        );
        assert!(session.current_folder().is_empty(), "should be at root");
    }

    #[test]
    fn request_disconnect_from_resting_emits_disconnect_and_walks_to_done() {
        let mut session = MasMceSession::new(
            Operation::PushMessage {
                folder: Folder::Outbox,
                bmessage: b"BEGIN:BMSG\r\nEND:BMSG\r\n".to_vec(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        session.set_keep_alive(true);
        run_push_into_resting(&mut session, 7);
        let bytes = session.request_disconnect().expect("DISCONNECT bytes");
        let parsed = parse_outbound(&bytes, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_DISCONNECT);
        assert_eq!(*session.state(), MasState::AwaitingDisconnect);

        let _ = session.handle_response(&ok_response(7, vec![]));
        assert_eq!(*session.state(), MasState::Done);
    }

    #[test]
    fn start_next_op_in_non_resting_state_errors() {
        let mut session = MasMceSession::new(
            Operation::PushMessage {
                folder: Folder::Outbox,
                bmessage: b"BEGIN:BMSG\r\nEND:BMSG\r\n".to_vec(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        // No CONNECT yet → still in Idle. Should refuse.
        let r = session.start_next_op(Operation::SetNotificationRegistration { enabled: true });
        assert!(r.is_err());
    }

    // -------------------------------------------------------------
    // SRM (Single Response Mode) — Pixel/Bluedroid PSE behaviour.
    // -------------------------------------------------------------

    /// Walk a freshly-constructed ListMessages session up to
    /// `AwaitingFirstGet` and return the parsed outbound GET packet.
    /// The connection-id is fixed at 0x1234 to match the OK-response
    /// helper.
    fn drive_to_first_get_listing(session: &mut MasMceSession) -> Packet {
        let _ = session.next_request().unwrap();
        let _ = session.handle_response(&ok_connect_response(0x1234));
        let _ = session.handle_response(&ok_response(0x1234, vec![])); // telecom
        let _ = session.handle_response(&ok_response(0x1234, vec![])); // msg
        let bytes = session
            .handle_response(&ok_response(0x1234, vec![])) // inbox → GET
            .expect("GET msg-listing");
        parse_outbound(&bytes, FixedPayload::None)
    }

    #[test]
    fn list_messages_get_advertises_srm_enable() {
        // We must announce SRM support on the first GET so the PSE
        // knows it's safe to flip on. Without this header Pixel falls
        // back to per-chunk continuations and works fine; with it,
        // Pixel chooses SRM and we MUST honour the flag in responses.
        let mut session = MasMceSession::new(
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 16,
                filter_message_type: MSG_TYPE_KEEP_BOTH_SMS_VARIANTS,
            },
            0,
        );
        let parsed = drive_to_first_get_listing(&mut session);
        assert_eq!(parsed.opcode, OP_GET_FINAL);
        let srm = parsed
            .header(obex::HDR_SRM)
            .expect("GET msg-listing must carry SRM=enable header");
        match srm {
            Header::Byte { value, .. } => assert_eq!(*value, 0x01),
            _ => panic!("SRM header must be a single byte"),
        }
    }

    #[test]
    fn srm_enable_in_response_pauses_get_continuations() {
        // Pixel/Bluedroid's MAS PSE flips SRM=enable on its first
        // CONTINUE for multi-packet listings, just like its PBAP PSE
        // does for phonebook chunks. If we keep sending GET
        // continuations after that, the PSE stalls partway through
        // the next OBEX packet (observed: ~100×127B UIH frames in
        // dlci 11, then total silence for the watchdog window).
        let mut session = MasMceSession::new(
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 16,
                filter_message_type: MSG_TYPE_KEEP_BOTH_SMS_VARIANTS,
            },
            0,
        );
        let _ = drive_to_first_get_listing(&mut session);
        assert_eq!(*session.state(), MasState::AwaitingFirstGet);

        // First CONTINUE arrives with SRM=enable + a body chunk.
        // We must NOT emit a continuation; the PSE will stream the
        // remainder back-to-back.
        let cont_with_srm = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0x1234),
                Header::Byte {
                    id: obex::HDR_SRM,
                    value: 0x01,
                },
                Header::body(b"<MAP-msg-listing version=\"1.0\">".to_vec()),
            ],
        };
        let next = session.handle_response(&cont_with_srm);
        assert!(
            next.is_none(),
            "SRM=enable means we wait for the next chunk silently"
        );
        assert_eq!(*session.state(), MasState::AwaitingContinuation);

        // Second CONTINUE arrives: still no outbound, still streaming.
        let cont2 = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0x1234),
                Header::body(b"<msg handle=\"AAAA\"/>".to_vec()),
            ],
        };
        let next = session.handle_response(&cont2);
        assert!(next.is_none(), "still under SRM, still silent");

        // Final OK with EndOfBody → DISCONNECT goes out as normal.
        let final_ok = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0x1234),
                Header::end_of_body(b"</MAP-msg-listing>".to_vec()),
            ],
        };
        let next = session.handle_response(&final_ok).unwrap();
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_DISCONNECT);
        match session.output() {
            OperationOutput::Listing(bytes) => {
                // Body assembler stitched all three chunks together.
                let want =
                    b"<MAP-msg-listing version=\"1.0\"><msg handle=\"AAAA\"/></MAP-msg-listing>";
                assert_eq!(bytes, want);
            }
            other => panic!("expected Listing output, got {:?}", other),
        }
    }

    #[test]
    fn no_srm_in_response_still_sends_get_continuations() {
        // Inverse of the SRM test — confirm the legacy non-SRM path
        // still works for servers that don't enable SRM. The PCE
        // must keep sending GET continuations until the PSE answers
        // with RSP_OK + EndOfBody.
        let mut session = MasMceSession::new(
            Operation::ListMessages {
                folder: Folder::Inbox,
                max_list_count: 16,
                filter_message_type: MSG_TYPE_KEEP_BOTH_SMS_VARIANTS,
            },
            0,
        );
        let _ = drive_to_first_get_listing(&mut session);

        let cont_no_srm = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0x1234),
                Header::body(b"<MAP-msg-listing version=\"1.0\"/>".to_vec()),
            ],
        };
        let next = session
            .handle_response(&cont_no_srm)
            .expect("non-SRM CONTINUE → GET continuation");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_GET_FINAL);
    }

    #[test]
    fn srm_active_resets_between_pooled_ops() {
        // The SRM latch is per-operation: Pixel re-asserts SRM on
        // every fresh GET, so when start_next_op flips us into a new
        // op we must clear the flag. Otherwise a previous SRM-active
        // op would silently suppress the *next* op's required GET
        // continuation if the new op happens to be non-SRM.
        let mut session = MasMceSession::new(
            Operation::GetMessage {
                folder: Folder::Inbox,
                handle: "AAAA".to_string(),
                charset: CHARSET_UTF8,
            },
            0,
        );
        session.set_keep_alive(true);
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![])); // telecom
        let _ = session.handle_response(&ok_response(7, vec![])); // msg
        let _ = session.handle_response(&ok_response(7, vec![])); // inbox → GET
                                                                  // PSE flips SRM on, then finishes in a single OK.
        let _ = session.handle_response(&Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(7),
                Header::Byte {
                    id: obex::HDR_SRM,
                    value: 0x01,
                },
                Header::end_of_body(b"first body".to_vec()),
            ],
        });
        assert_eq!(*session.state(), MasState::Resting);
        assert!(session.srm_active, "SRM was latched mid-op");

        // Queue a second GET in the same folder — start_next_op must
        // clear srm_active so the next op gets to negotiate fresh.
        let _ = session
            .start_next_op(Operation::GetMessage {
                folder: Folder::Inbox,
                handle: "BBBB".to_string(),
                charset: CHARSET_UTF8,
            })
            .expect("start_next_op packet");
        assert!(
            !session.srm_active,
            "start_next_op must clear srm_active so the new op can negotiate from scratch"
        );
    }
}
