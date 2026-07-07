//! MAP MNS server — receives EventReport notifications pushed by the
//! phone when new messages arrive.
//!
//! Phase 4d of the SMS auto-reply plan. This is a *server*: the phone
//! initiates the OBEX session and sends us PUTs, we just respond. The
//! flow (per MAP §3.7):
//!
//! ```text
//! ← CONNECT(Target = MNS UUID)
//! → OK + Connection-Id + Who(MNS UUID)
//! ← PUT(final, Type = x-bt/MAP-event-report, EndOfBody = <xml>)
//! → OK
//! ← DISCONNECT
//! → OK
//! ```
//!
//! The phone may keep the OBEX session open across multiple events, so
//! the server must accept `PUT → OK → PUT → OK` cycles without seeing a
//! DISCONNECT in between.
//!
//! Multi-packet PUT bodies (Body...Body...EndOfBody) are reassembled
//! via `BodyAssembler`; for SMS notifications a single packet with an
//! EndOfBody header is the norm but we handle the continuation case for
//! robustness.
//!
//! The XML body is parsed in this same module — see `parse_event_report`
//! and `EventReportEvent`. We expose the raw XML alongside the parsed
//! event so a future caller (logging, UI debugging) can see what the AG
//! actually sent.

#![allow(dead_code)] // Phase 4d — runtime integration follows in 4e.

use super::obex::{
    build_connect_response, BodyAssembler, FixedPayload, Header, Packet, HDR_CONNECTION_ID,
    HDR_TARGET, HDR_TYPE, OP_CONNECT, OP_DISCONNECT, OP_PUT, RSP_BAD_REQUEST, RSP_CONTINUE,
    RSP_NOT_FOUND, RSP_NOT_IMPLEMENTED, RSP_OK, RSP_SERVICE_UNAVAILABLE,
};

/// MNS target UUID (MAP §3.2.1). Big-endian on the wire. Spec value:
/// `bb582b41-420c-11db-b0de-0800200c9a66`.
pub const MNS_TARGET_UUID: [u8; 16] = [
    0xbb, 0x58, 0x2b, 0x41, 0x42, 0x0c, 0x11, 0xdb, 0xb0, 0xde, 0x08, 0x00, 0x20, 0x0c, 0x9a, 0x66,
];

/// Largest packet we'll accept inbound. Same as MAS / PBAP.
pub const DEFAULT_MAX_PACKET_LENGTH: u16 = 0x4000;

/// OBEX `Type:` string the phone uses on every notification PUT.
pub const TYPE_EVENT_REPORT: &str = "x-bt/MAP-event-report";

/// Connection-Id we assign in our CONNECT response. Servers pick this
/// freely; using a fixed sentinel makes log-spelunking easier and
/// nothing on the wire requires uniqueness across reconnects.
const ASSIGNED_CONNECTION_ID: u32 = 0xa0_4c_1e_01;

/// What the server is currently waiting for. Public so a runtime can
/// log/inspect the lifecycle, but transitions are driven entirely by
/// `handle_request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MnsState {
    /// Initial: expecting OBEX CONNECT.
    AwaitingConnect,
    /// CONNECT accepted; ready to receive PUTs and DISCONNECT.
    Connected,
    /// Mid-PUT; reassembling the body across packets. The phone has
    /// sent at least one non-final PUT (Body header) and we replied
    /// CONTINUE.
    AssemblingPut,
    /// Terminal: phone sent DISCONNECT or a fatal error occurred.
    Disconnected,
    /// Terminal: protocol violation. The runtime should drop the
    /// RFCOMM channel.
    Failed(String),
}

/// One parsed inbound event. Anything other than NewMessage we surface
/// as `Other` so callers can log it without us having to enumerate the
/// full event type list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MnsEvent {
    /// A new SMS (or other supported message type) arrived. The
    /// runtime can now spawn a `MapRuntime` to fetch the bMessage by
    /// `handle`.
    NewMessage {
        handle: String,
        folder: Option<String>,
        msg_type: Option<String>,
    },
    /// Some other event the phone reported (DeliverySuccess,
    /// MemoryFull, MessageDeleted, etc.). We log and forget; SMS
    /// auto-reply doesn't need to act on these.
    Other {
        event_type: String,
        handle: Option<String>,
    },
    /// The PUT body wasn't an EventReport we could parse. Surfacing
    /// it as a distinct variant keeps log-callers from having to dig
    /// into the raw bytes.
    Unparseable { reason: String },
}

#[derive(Debug)]
pub struct MnsServer {
    state: MnsState,
    connection_id: Option<u32>,
    body: BodyAssembler,
    pending_events: Vec<MnsEvent>,
    /// Reassembly buffer for OBEX packets arriving as RFCOMM UIH
    /// fragments. The caller's RFCOMM layer hands us payload bytes
    /// without OBEX framing semantics, so we glue them back together
    /// here using the same 3-byte BE length header `pbap_runtime`
    /// reads on its outbound channels.
    obex_buffer: Vec<u8>,
}

impl MnsServer {
    pub fn new() -> Self {
        Self {
            state: MnsState::AwaitingConnect,
            connection_id: None,
            body: BodyAssembler::new(),
            pending_events: Vec::new(),
            obex_buffer: Vec::new(),
        }
    }

    /// Reset to AwaitingConnect so a fresh RFCOMM session from a
    /// reconnecting phone starts clean. Called by the runtime on
    /// RFCOMM DISC.
    pub fn reset(&mut self) {
        self.state = MnsState::AwaitingConnect;
        self.connection_id = None;
        self.body = BodyAssembler::new();
        self.obex_buffer.clear();
        // Pending events deliberately preserved so the runtime can
        // still drain a NewMessage that came in immediately before
        // the disconnect.
    }

    pub fn state(&self) -> &MnsState {
        &self.state
    }

    pub fn take_events(&mut self) -> Vec<MnsEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Feed raw RFCOMM UIH payload bytes. Reassembles them into
    /// complete OBEX packets via the 3-byte BE length header and
    /// dispatches each through `handle_request`. Returns the OBEX
    /// response packet bytes for every complete request — the caller
    /// (RFCOMM layer) wraps each one in a UIH frame and writes it.
    ///
    /// Stops at the first malformed packet and surfaces it as an
    /// error reply; the runtime should then tear the channel down.
    pub fn feed_bytes(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.obex_buffer.extend_from_slice(bytes);
        println!(
            "[AokieRadio] MNS feed_bytes +{}B (buf now {}B, state={:?})",
            bytes.len(),
            self.obex_buffer.len(),
            self.state
        );
        let mut out = Vec::new();
        loop {
            let take = match try_take_obex_packet(&self.obex_buffer) {
                Ok(Some(n)) => n,
                Ok(None) => break,
                Err(reason) => {
                    println!("[AokieRadio] MNS OBEX header malformed: {}", reason);
                    self.state = MnsState::Failed(format!("OBEX header malformed: {}", reason));
                    out.push(build_simple_response(RSP_BAD_REQUEST));
                    break;
                }
            };
            let packet_bytes: Vec<u8> = self.obex_buffer.drain(..take).collect();
            println!(
                "[AokieRadio] MNS OBEX packet ready: {}B opcode=0x{:02x}",
                packet_bytes.len(),
                packet_bytes[0]
            );
            // The OBEX FixedPayload depends on which kind of request
            // we expect. CONNECT has 4 bytes of fixed payload; nothing
            // else does. The phone is the requester so the *only*
            // CONNECT shape that arrives here is a request, never a
            // response.
            let fixed = if (packet_bytes[0] & 0x7f) == (OP_CONNECT & 0x7f) {
                FixedPayload::Connect
            } else {
                FixedPayload::None
            };
            let parsed = match Packet::parse(&packet_bytes, fixed) {
                Ok(p) => p,
                Err(reason) => {
                    self.state = MnsState::Failed(format!("OBEX parse failed: {}", reason));
                    out.push(build_simple_response(RSP_BAD_REQUEST));
                    break;
                }
            };
            out.push(self.handle_request(&parsed));
        }
        out
    }

    /// Process one inbound OBEX request. Returns the bytes to send
    /// back to the phone. On a fatal error the response carries an
    /// error opcode and `state` transitions to `Failed`; the caller
    /// should still write the bytes (so the phone sees the error)
    /// and then tear the RFCOMM channel down.
    pub fn handle_request(&mut self, packet: &Packet) -> Vec<u8> {
        let opcode_no_final = packet.opcode & 0x7f;
        let is_final = packet.opcode & 0x80 != 0;
        match opcode_no_final {
            op if op == (OP_CONNECT & 0x7f) => self.handle_connect(packet),
            op if op == (OP_DISCONNECT & 0x7f) => self.handle_disconnect(packet),
            // PUT (0x02 base; final bit distinguishes 0x02 vs 0x82).
            op if op == OP_PUT => self.handle_put(packet, is_final),
            other => {
                eprintln!(
                    "[MNS] unexpected OBEX opcode 0x{:02x} — replying NOT_IMPLEMENTED",
                    other
                );
                build_simple_response(RSP_NOT_IMPLEMENTED)
            }
        }
    }

    fn handle_connect(&mut self, packet: &Packet) -> Vec<u8> {
        if !matches!(self.state, MnsState::AwaitingConnect) {
            // Phones occasionally re-CONNECT to recover — accept it
            // by re-issuing the same Connection-Id rather than
            // rejecting. State transitions back to Connected.
            eprintln!(
                "[MNS] CONNECT received in state {:?} — re-issuing connection id",
                self.state
            );
        }
        let target_ok = match packet.header(HDR_TARGET) {
            Some(Header::ByteSeq { value, .. }) => value.as_slice() == MNS_TARGET_UUID,
            _ => false,
        };
        if !target_ok {
            let raw_target = packet.header(HDR_TARGET).map(|h| match h {
                Header::ByteSeq { value, .. } => format!("{} bytes: {:02x?}", value.len(), value),
                _ => format!("{:?}", h),
            });
            println!(
                "[AokieRadio] MNS CONNECT rejected — Target UUID mismatch (got {:?}); \
                 phone won't push notifications",
                raw_target
            );
            self.state = MnsState::Failed("CONNECT had wrong/missing Target UUID".to_string());
            // CONNECT responses (success or failure) must carry the
            // 4-byte fixed payload (version/flags/max). Build a real
            // CONNECT response so the parser on the other end doesn't
            // choke trying to read a fixed-payload that isn't there.
            return build_connect_response(RSP_BAD_REQUEST, DEFAULT_MAX_PACKET_LENGTH, None, None);
        }
        self.connection_id = Some(ASSIGNED_CONNECTION_ID);
        self.state = MnsState::Connected;
        println!(
            "[AokieRadio] MNS CONNECT accepted — state=Connected, ready for NewMessage pushes"
        );
        build_connect_response(
            RSP_OK,
            DEFAULT_MAX_PACKET_LENGTH,
            Some(ASSIGNED_CONNECTION_ID),
            Some(MNS_TARGET_UUID.to_vec()),
        )
    }

    fn handle_disconnect(&mut self, _packet: &Packet) -> Vec<u8> {
        self.state = MnsState::Disconnected;
        build_simple_response(RSP_OK)
    }

    fn handle_put(&mut self, packet: &Packet, is_final: bool) -> Vec<u8> {
        println!(
            "[AokieRadio] MNS PUT received final={} state={:?} headers={}",
            is_final,
            self.state,
            packet.headers.len()
        );
        if !matches!(self.state, MnsState::Connected | MnsState::AssemblingPut) {
            println!(
                "[AokieRadio] MNS PUT rejected — state {:?} not ready",
                self.state
            );
            return build_simple_response(RSP_SERVICE_UNAVAILABLE);
        }
        // Validate Connection-Id matches what we issued.
        let conn_id = packet.header(HDR_CONNECTION_ID).and_then(|h| match h {
            Header::Quad { value, .. } => Some(*value),
            _ => None,
        });
        if conn_id != self.connection_id {
            println!(
                "[AokieRadio] MNS PUT bad connection-id {:?} (issued {:?})",
                conn_id, self.connection_id
            );
            self.state = MnsState::Failed(format!(
                "PUT Connection-Id {:?} does not match issued {:?}",
                conn_id, self.connection_id
            ));
            return build_simple_response(RSP_BAD_REQUEST);
        }
        // Type only appears on the first PUT request of a chained
        // operation (OBEX 1.5 §3.6.2). Continuation packets carry
        // Connection-Id + Body / EndOfBody only — re-checking Type on
        // every chunk wrongly rejects them with NOT_FOUND. Validate
        // only when starting a new PUT (state still Connected).
        if matches!(self.state, MnsState::Connected) {
            let type_matches = match packet.header(HDR_TYPE) {
                Some(Header::ByteSeq { value, .. }) => {
                    // Trim trailing null per OBEX Type encoding.
                    let trimmed = value.split(|b| *b == 0).next().unwrap_or(&[]);
                    trimmed == TYPE_EVENT_REPORT.as_bytes()
                }
                _ => false,
            };
            if !type_matches {
                let raw_type = packet.header(HDR_TYPE).map(|h| match h {
                    Header::ByteSeq { value, .. } => String::from_utf8_lossy(value).to_string(),
                    _ => format!("{:?}", h),
                });
                println!(
                    "[AokieRadio] MNS PUT type mismatch (expected '{}', got {:?}) — replying NOT_FOUND",
                    TYPE_EVENT_REPORT, raw_type
                );
                return build_simple_response(RSP_NOT_FOUND);
            }
        }
        // Reassemble the body.
        if let Err(reason) = self.body.push(packet) {
            println!(
                "[AokieRadio] MNS body assembler rejected packet: {}",
                reason
            );
            self.state = MnsState::Failed(reason.clone());
            return build_simple_response(RSP_BAD_REQUEST);
        }
        if !is_final {
            self.state = MnsState::AssemblingPut;
            println!("[AokieRadio] MNS PUT non-final → CONTINUE (assembled body so far)");
            return build_simple_response(RSP_CONTINUE);
        }
        // Final packet — body must now be complete (EndOfBody seen).
        if !self.body.is_complete() {
            // Some phones send PUT_FINAL with only Body (no
            // EndOfBody). Per spec that's malformed but we tolerate
            // it: treat the final packet's accumulated bytes as the
            // body and push the event.
            println!("[AokieRadio] MNS PUT_FINAL without EndOfBody — accepting accumulated body");
        }
        let body_bytes = std::mem::take(&mut self.body).into_bytes();
        // The MNS event-report XML can include sender_phone /
        // sender_name attributes — keep the body length visible (useful
        // when debugging non-arriving notifications) but redact the
        // payload itself unless the user has explicitly opted into
        // verbose logs.
        let body_str = String::from_utf8_lossy(&body_bytes);
        println!(
            "[AokieRadio] MNS PUT_FINAL body {}B: {}",
            body_bytes.len(),
            aokie_core::redact::Text(&body_str)
        );
        let event = parse_event_report(&body_bytes);
        println!("[AokieRadio] MNS parsed event: {:?}", event);
        self.pending_events.push(event);
        // Body assembler reset so the next PUT starts fresh.
        self.body = BodyAssembler::new();
        self.state = MnsState::Connected;
        build_simple_response(RSP_OK)
    }
}

impl Default for MnsServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a MAP EventReport XML document. Permissive: missing fields
/// become `None`, malformed input collapses to `Unparseable`. We don't
/// use a real XML crate — the event element is flat with all data in
/// attributes (same shape as the MAP listing parser handles).
pub fn parse_event_report(input: &[u8]) -> MnsEvent {
    let text = String::from_utf8_lossy(input);
    let bytes = text.as_bytes();
    let Some(start) = find_event_open_tag(bytes) else {
        return MnsEvent::Unparseable {
            reason: "no <event> element".to_string(),
        };
    };
    let after = start + b"<event".len();
    let Some(end) = bytes[after..].iter().position(|b| *b == b'>') else {
        return MnsEvent::Unparseable {
            reason: "<event> tag was never closed".to_string(),
        };
    };
    let attr_section = &text[after..after + end];
    let mut event_type: Option<String> = None;
    let mut handle: Option<String> = None;
    let mut folder: Option<String> = None;
    let mut msg_type: Option<String> = None;
    for (name, value) in iter_attributes(attr_section) {
        match name {
            "type" => event_type = Some(value.to_string()),
            "handle" => handle = Some(value.to_string()),
            "folder" => folder = Some(value.to_string()),
            "msg_type" => msg_type = Some(value.to_string()),
            _ => {}
        }
    }
    match (event_type.as_deref(), handle) {
        (Some("NewMessage"), Some(handle)) => MnsEvent::NewMessage {
            handle,
            folder,
            msg_type,
        },
        (Some(other_type), handle) => MnsEvent::Other {
            event_type: other_type.to_string(),
            handle,
        },
        (None, _) => MnsEvent::Unparseable {
            reason: "<event> missing type attribute".to_string(),
        },
    }
}

/// Find the start of an `<event` open tag (not `</event>`). Mirrors
/// the same care `map_listing.rs` takes — must not match substrings
/// like `<events>` or `</event>`.
fn find_event_open_tag(haystack: &[u8]) -> Option<usize> {
    let needle = b"<event";
    let mut cursor = 0;
    while cursor + needle.len() <= haystack.len() {
        if let Some(rel) = haystack[cursor..]
            .windows(needle.len())
            .position(|w| w == needle)
        {
            let start = cursor + rel;
            let after = start + needle.len();
            if after >= haystack.len() {
                return None;
            }
            let next = haystack[after];
            // Tag-name terminator: whitespace, `/`, or `>`.
            if next.is_ascii_whitespace() || next == b'/' || next == b'>' {
                return Some(start);
            }
            cursor = after;
        } else {
            return None;
        }
    }
    None
}

/// Iterate `name="value"` attribute pairs out of an attribute section.
/// Same shape as `map_listing::parse_msg_attributes` but yields
/// (name, value) tuples instead of populating a struct.
fn iter_attributes(section: &str) -> impl Iterator<Item = (&str, &str)> + '_ {
    let mut chars = section.char_indices().peekable();
    std::iter::from_fn(move || loop {
        let (i, c) = *chars.peek()?;
        if c.is_ascii_whitespace() || c == '/' {
            chars.next();
            continue;
        }
        let name_start = i;
        let mut name_end = name_start;
        while let Some(&(j, ch)) = chars.peek() {
            if ch == '=' || ch.is_ascii_whitespace() {
                name_end = j;
                break;
            }
            chars.next();
            name_end = j + ch.len_utf8();
        }
        while let Some(&(_, ch)) = chars.peek() {
            if ch.is_ascii_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        if let Some(&(_, '=')) = chars.peek() {
            chars.next();
        } else {
            return None;
        }
        while let Some(&(_, ch)) = chars.peek() {
            if ch.is_ascii_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        let quote = match chars.peek().copied() {
            Some((_, q)) if q == '"' || q == '\'' => {
                chars.next();
                q
            }
            _ => return None,
        };
        let value_start = chars.peek().map(|&(j, _)| j).unwrap_or(section.len());
        let mut value_end = value_start;
        loop {
            match chars.next() {
                Some((j, ch)) if ch == quote => {
                    value_end = j;
                    break;
                }
                Some((j, ch)) => {
                    value_end = j + ch.len_utf8();
                }
                None => break,
            }
        }
        return Some((
            &section[name_start..name_end],
            &section[value_start..value_end],
        ));
    })
}

/// Inspect the OBEX byte stream and return the number of bytes that
/// constitute the next complete packet, or `None` if more bytes are
/// needed. Mirrors `pbap_runtime::try_take_obex_packet` — we duplicate
/// it here so map_mns stays I/O-free and doesn't depend on the runtime
/// crate.
fn try_take_obex_packet(buffer: &[u8]) -> Result<Option<usize>, String> {
    if buffer.len() < 3 {
        return Ok(None);
    }
    let len = u16::from_be_bytes([buffer[1], buffer[2]]) as usize;
    if len < 3 {
        return Err(format!(
            "OBEX packet declares length {} (must be >= 3 for the header)",
            len
        ));
    }
    if buffer.len() < len {
        return Ok(None);
    }
    Ok(Some(len))
}

/// Build a minimal OBEX response with just an opcode (no headers, no
/// fixed payload). Used for plain OK / CONTINUE / error responses.
fn build_simple_response(opcode: u8) -> Vec<u8> {
    Packet {
        opcode,
        fixed_payload: Vec::new(),
        headers: Vec::new(),
    }
    .encode()
}

// =============================================================================
// Tests.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aokie_radio::obex::{
        build_connect_request, build_disconnect, build_put_request, OP_ABORT,
    };

    /// Round-trip helper: encode the inbound bytes back through the
    /// OBEX parser with the right `FixedPayload` so we can re-feed
    /// arbitrary requests we built ourselves into the server.
    fn parse_inbound(bytes: &[u8], fixed: FixedPayload) -> Packet {
        Packet::parse(bytes, fixed).expect("inbound packet must round-trip")
    }

    #[test]
    fn mns_target_uuid_matches_spec() {
        // Pin so a future "tidy these up" change can't silently
        // change what we negotiate against. Bytes from MAP 1.4.2
        // §3.2.1.
        assert_eq!(
            MNS_TARGET_UUID,
            [
                0xbb, 0x58, 0x2b, 0x41, 0x42, 0x0c, 0x11, 0xdb, 0xb0, 0xde, 0x08, 0x00, 0x20, 0x0c,
                0x9a, 0x66
            ]
        );
        // Easy off-by-one trap: byte 4 is 0x41 for MNS, 0x40 for MAS.
        assert_eq!(MNS_TARGET_UUID[3], 0x41);
    }

    #[test]
    fn connect_with_correct_target_returns_ok_with_connection_id_and_who() {
        let mut server = MnsServer::new();
        let req_bytes = build_connect_request(Some(&MNS_TARGET_UUID), DEFAULT_MAX_PACKET_LENGTH);
        let req = parse_inbound(&req_bytes, FixedPayload::Connect);
        let resp_bytes = server.handle_request(&req);
        let resp = Packet::parse(&resp_bytes, FixedPayload::Connect).unwrap();
        assert_eq!(resp.opcode, RSP_OK);
        let conn = resp.header(HDR_CONNECTION_ID).expect("Connection-Id");
        match conn {
            Header::Quad { value, .. } => assert_eq!(*value, ASSIGNED_CONNECTION_ID),
            _ => panic!("Connection-Id must be Quad"),
        }
        // State should have moved past AwaitingConnect.
        assert_eq!(server.state(), &MnsState::Connected);
    }

    #[test]
    fn connect_with_wrong_target_collapses_to_failed_and_returns_bad_request() {
        let mut server = MnsServer::new();
        // Use the MAS UUID (byte 3 = 0x40) — common confusion in the
        // wild and exactly the case we want to reject loudly.
        let mas_uuid = [
            0xbb, 0x58, 0x2b, 0x40, 0x42, 0x0c, 0x11, 0xdb, 0xb0, 0xde, 0x08, 0x00, 0x20, 0x0c,
            0x9a, 0x66,
        ];
        let req_bytes = build_connect_request(Some(&mas_uuid), DEFAULT_MAX_PACKET_LENGTH);
        let req = parse_inbound(&req_bytes, FixedPayload::Connect);
        let resp_bytes = server.handle_request(&req);
        let resp = Packet::parse(&resp_bytes, FixedPayload::Connect).unwrap();
        assert_eq!(resp.opcode, RSP_BAD_REQUEST);
        assert!(matches!(server.state(), MnsState::Failed(_)));
    }

    fn established_server() -> MnsServer {
        let mut server = MnsServer::new();
        let req_bytes = build_connect_request(Some(&MNS_TARGET_UUID), DEFAULT_MAX_PACKET_LENGTH);
        let req = parse_inbound(&req_bytes, FixedPayload::Connect);
        let _ = server.handle_request(&req);
        server
    }

    #[test]
    fn put_with_event_report_xml_emits_new_message_event_and_returns_ok() {
        let mut server = established_server();
        let xml = br#"<MAP-event-report version="1.0">
            <event type="NewMessage"
                   handle="0123456789ABCDEF"
                   folder="telecom/msg/inbox"
                   msg_type="SMS_GSM" />
        </MAP-event-report>"#;
        let extras = vec![Header::obex_type(TYPE_EVENT_REPORT)];
        let req_bytes = build_put_request(ASSIGNED_CONNECTION_ID, extras, xml.to_vec());
        let req = parse_inbound(&req_bytes, FixedPayload::None);
        let resp_bytes = server.handle_request(&req);
        let resp = Packet::parse(&resp_bytes, FixedPayload::None).unwrap();
        assert_eq!(resp.opcode, RSP_OK);
        let events = server.take_events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            MnsEvent::NewMessage {
                handle,
                folder,
                msg_type,
            } => {
                assert_eq!(handle, "0123456789ABCDEF");
                assert_eq!(folder.as_deref(), Some("telecom/msg/inbox"));
                assert_eq!(msg_type.as_deref(), Some("SMS_GSM"));
            }
            other => panic!("expected NewMessage, got {:?}", other),
        }
        // Server stays Connected so the next PUT is accepted.
        assert_eq!(server.state(), &MnsState::Connected);
    }

    #[test]
    fn put_with_unknown_type_returns_not_found_without_event() {
        let mut server = established_server();
        let extras = vec![Header::obex_type("x-bt/unknown")];
        let req_bytes =
            build_put_request(ASSIGNED_CONNECTION_ID, extras, b"<irrelevant/>".to_vec());
        let req = parse_inbound(&req_bytes, FixedPayload::None);
        let resp = Packet::parse(&server.handle_request(&req), FixedPayload::None).unwrap();
        assert_eq!(resp.opcode, RSP_NOT_FOUND);
        assert!(server.take_events().is_empty());
    }

    #[test]
    fn put_with_wrong_connection_id_collapses_to_failed() {
        let mut server = established_server();
        let extras = vec![Header::obex_type(TYPE_EVENT_REPORT)];
        let req_bytes = build_put_request(0xdead_beef, extras, b"<irrelevant/>".to_vec());
        let req = parse_inbound(&req_bytes, FixedPayload::None);
        let resp = Packet::parse(&server.handle_request(&req), FixedPayload::None).unwrap();
        assert_eq!(resp.opcode, RSP_BAD_REQUEST);
        assert!(matches!(server.state(), MnsState::Failed(_)));
    }

    #[test]
    fn delivery_success_event_collapses_to_other_variant() {
        let mut server = established_server();
        let xml = br#"<MAP-event-report>
            <event type="DeliverySuccess" handle="0001" />
        </MAP-event-report>"#;
        let extras = vec![Header::obex_type(TYPE_EVENT_REPORT)];
        let req_bytes = build_put_request(ASSIGNED_CONNECTION_ID, extras, xml.to_vec());
        let req = parse_inbound(&req_bytes, FixedPayload::None);
        let _ = server.handle_request(&req);
        let events = server.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            MnsEvent::Other { event_type, handle: Some(h) }
            if event_type == "DeliverySuccess" && h == "0001"
        ));
    }

    #[test]
    fn malformed_xml_collapses_to_unparseable_event() {
        let mut server = established_server();
        let xml = b"not even xml";
        let extras = vec![Header::obex_type(TYPE_EVENT_REPORT)];
        let req_bytes = build_put_request(ASSIGNED_CONNECTION_ID, extras, xml.to_vec());
        let req = parse_inbound(&req_bytes, FixedPayload::None);
        // Even on garbage XML we still respond OK to the phone — the
        // alternative (replying error) would just cause it to retry
        // indefinitely.
        let resp = Packet::parse(&server.handle_request(&req), FixedPayload::None).unwrap();
        assert_eq!(resp.opcode, RSP_OK);
        let events = server.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], MnsEvent::Unparseable { .. }));
    }

    #[test]
    fn disconnect_returns_ok_and_terminates_session() {
        let mut server = established_server();
        let req_bytes = build_disconnect(Some(ASSIGNED_CONNECTION_ID));
        let req = parse_inbound(&req_bytes, FixedPayload::None);
        let resp = Packet::parse(&server.handle_request(&req), FixedPayload::None).unwrap();
        assert_eq!(resp.opcode, RSP_OK);
        assert_eq!(server.state(), &MnsState::Disconnected);
    }

    #[test]
    fn unknown_opcode_returns_not_implemented_without_state_change() {
        let mut server = established_server();
        // Build a fake "ABORT" — we don't accept it but mustn't crash.
        let req = Packet {
            opcode: OP_ABORT,
            fixed_payload: Vec::new(),
            headers: vec![Header::connection_id(ASSIGNED_CONNECTION_ID)],
        };
        let resp = Packet::parse(&server.handle_request(&req), FixedPayload::None).unwrap();
        assert_eq!(resp.opcode, RSP_NOT_IMPLEMENTED);
        // Still in Connected — unknown opcodes are non-fatal.
        assert_eq!(server.state(), &MnsState::Connected);
    }

    #[test]
    fn parse_event_report_handles_singletag_self_closing_form() {
        // Some phones emit <event ... /> with no whitespace before /.
        let xml =
            br#"<MAP-event-report><event type="NewMessage" handle="0001"/></MAP-event-report>"#;
        let event = parse_event_report(xml);
        assert!(matches!(
            event,
            MnsEvent::NewMessage { ref handle, .. } if handle == "0001"
        ));
    }

    #[test]
    fn feed_bytes_reassembles_split_packets_across_uih_fragments() {
        // Simulate an RFCOMM layer that hands us a CONNECT in two
        // arbitrary chunks (mid-fixed-payload split). The server must
        // hold its response until the full packet is in.
        let mut server = MnsServer::new();
        let connect_bytes =
            build_connect_request(Some(&MNS_TARGET_UUID), DEFAULT_MAX_PACKET_LENGTH);
        let split = 5; // mid-CONNECT, after opcode+length but in the fixed payload
        let first = server.feed_bytes(&connect_bytes[..split]);
        assert!(first.is_empty(), "no response until full packet arrives");
        let rest = server.feed_bytes(&connect_bytes[split..]);
        assert_eq!(rest.len(), 1, "one OK response after final byte");
        let resp = Packet::parse(&rest[0], FixedPayload::Connect).unwrap();
        assert_eq!(resp.opcode, RSP_OK);
    }

    #[test]
    fn feed_bytes_can_handle_back_to_back_packets_in_one_chunk() {
        // Some phones merge CONNECT + first PUT into a single UIH
        // frame. Parser should drain both.
        let mut server = MnsServer::new();
        let connect = build_connect_request(Some(&MNS_TARGET_UUID), DEFAULT_MAX_PACKET_LENGTH);
        let xml =
            br#"<MAP-event-report><event type="NewMessage" handle="0001" /></MAP-event-report>"#;
        let put = build_put_request(
            ASSIGNED_CONNECTION_ID,
            vec![Header::obex_type(TYPE_EVENT_REPORT)],
            xml.to_vec(),
        );
        let mut combined = connect;
        combined.extend_from_slice(&put);
        let responses = server.feed_bytes(&combined);
        assert_eq!(responses.len(), 2, "one OK per request");
        // Second response (to the PUT) must be plain OK and the
        // server must have surfaced the NewMessage event.
        let second = Packet::parse(&responses[1], FixedPayload::None).unwrap();
        assert_eq!(second.opcode, RSP_OK);
        let events = server.take_events();
        assert!(matches!(events.as_slice(), [MnsEvent::NewMessage { .. }]));
    }

    #[test]
    fn parse_event_report_returns_unparseable_when_no_event_element() {
        let xml = b"<MAP-event-report />";
        let event = parse_event_report(xml);
        assert!(matches!(event, MnsEvent::Unparseable { .. }));
    }

    #[test]
    fn multi_packet_put_accepts_continuation_without_repeated_type() {
        // Pixel Bluedroid splits the EventReport PUT across two OBEX
        // packets: PUT (not-final) carries Type + Body fragment;
        // PUT_FINAL carries only Connection-Id + EndOfBody. Per OBEX
        // §3.6.2 Type only appears on the first request of a chained
        // operation. Re-checking it on the final chunk wrongly
        // returned NOT_FOUND and Pixel tore down the MNS RFCOMM
        // channel. Regression coverage so we don't backslide.
        use crate::aokie_radio::obex::{Packet as ObexPacket, OP_PUT, OP_PUT_FINAL};
        let mut server = established_server();
        let xml = br#"<MAP-event-report version="1.0">
            <event type="NewMessage" handle="ABCD" folder="telecom/msg/inbox" msg_type="SMS_GSM" />
        </MAP-event-report>"#;
        let split = xml.len() / 2;
        // Chunk 1: not-final, Type + Body fragment.
        let put_first = ObexPacket {
            opcode: OP_PUT,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(ASSIGNED_CONNECTION_ID),
                Header::obex_type(TYPE_EVENT_REPORT),
                Header::body(xml[..split].to_vec()),
            ],
        }
        .encode();
        let req1 = parse_inbound(&put_first, FixedPayload::None);
        let resp1 = ObexPacket::parse(&server.handle_request(&req1), FixedPayload::None).unwrap();
        assert_eq!(resp1.opcode, RSP_CONTINUE, "first chunk → CONTINUE");
        // Chunk 2: final, NO Type — only Connection-Id + EndOfBody.
        let put_final = ObexPacket {
            opcode: OP_PUT_FINAL,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(ASSIGNED_CONNECTION_ID),
                Header::end_of_body(xml[split..].to_vec()),
            ],
        }
        .encode();
        let req2 = parse_inbound(&put_final, FixedPayload::None);
        let resp2 = ObexPacket::parse(&server.handle_request(&req2), FixedPayload::None).unwrap();
        assert_eq!(
            resp2.opcode, RSP_OK,
            "final chunk without Type must be accepted (Type was on first chunk)"
        );
        let events = server.take_events();
        assert!(
            matches!(events.as_slice(), [MnsEvent::NewMessage { handle, .. }] if handle == "ABCD"),
            "reassembled body must parse as NewMessage; got {:?}",
            events
        );
    }

    #[test]
    fn fuzz_event_report_does_not_panic_on_random_bytes() {
        // MNS event reports come in over OBEX from the phone; the
        // body is XML-shaped but real MAP servers are lax about it.
        // Random bytes (and invalid UTF-8) must never crash us.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x4e4d_535f_465a_5a31);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 512) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_event_report(&buf);
        }
    }
}
