//! PBAP PCE (Phonebook Access Profile — Client Equipment) driver.
//!
//! Phase 1c of the MAP/PBAP plan. Implements a synchronous state
//! machine on top of `obex` and `vcard`. The runtime feeds it incoming
//! OBEX response packets and asks for the next outbound packet via
//! `next_request`; everything I/O-shaped happens outside this module.
//!
//! Flow (happy path):
//!
//! ```text
//! → CONNECT(Target = PBAP UUID, max_packet_length)
//! ← OK + Connection-Id + Who(PBAP UUID)
//! → SETPATH(Name = "telecom")
//! ← OK
//! → SETPATH(Name = "pb")
//! ← OK
//! → GET(Type = "x-bt/phonebook", Name = "telecom/pb.vcf", AppParams)
//! ← CONTINUE + Body(...)
//! → GET continuation
//! ← CONTINUE + Body(...)
//! → GET continuation
//! ← OK + EndOfBody(...)
//! → DISCONNECT
//! ← OK
//! ```
//!
//! Errors at any step (CONNECT refused, SETPATH `pb` not found,
//! phonebook empty, etc.) collapse into `PbapState::Failed(reason)`
//! so the caller can surface a user-visible message and tear the
//! RFCOMM channel down. The session does not retry — that's the
//! caller's policy choice.
//!
//! What this module is **not** responsible for:
//!   - Outbound RFCOMM connection setup (we're a fresh tenant on a
//!     channel the runtime opens for us).
//!   - SDP discovery of the phone's PBAP server channel — runtime job.
//!   - SQLite persistence of the resulting contacts — that lives in
//!     `commands::pbap_commands` (Phase 1e).
//!
//! Reference: PBAP 1.2.3 §5 (Generic Object Exchange), §6.3.4
//! (PullPhoneBook). Cross-checked against `nccgroup/nOBEX` for the
//! PBAP target UUID and SETPATH path traversal.

#![allow(dead_code)] // Phase 1c — runtime integration follows.

use super::obex;
use super::obex::{
    build_connect_request, build_disconnect, build_get_continuation, build_get_request,
    build_setpath, BodyAssembler, Header, Packet, HDR_CONNECTION_ID, RSP_CONTINUE, RSP_OK,
};
use super::vcard::{parse_vcards, Contact};

/// PBAP PSE service UUID — big-endian on the wire. Spec value:
/// `796135f0-f0c5-11d8-0966-0800200c9a66`.
pub const PBAP_TARGET_UUID: [u8; 16] = [
    0x79, 0x61, 0x35, 0xf0, 0xf0, 0xc5, 0x11, 0xd8, 0x09, 0x66, 0x08, 0x00, 0x20, 0x0c, 0x9a, 0x66,
];

/// We advertise this as the largest packet we can accept inbound. The
/// PSE responds with its own preference; the smaller of the two is the
/// negotiated MTU. 0x4000 (16 KB) is what most Android implementations
/// emit and is comfortably above any single phonebook entry.
pub const DEFAULT_MAX_PACKET_LENGTH: u16 = 0x4000;

/// Standard PBAP phonebook object type — the same string Android phones
/// expect verbatim, null-terminator included by the OBEX Type encoder.
pub const TYPE_PHONEBOOK: &str = "x-bt/phonebook";

/// What the session is currently waiting on. The runtime drives this
/// state by calling `next_request` and `handle_response` alternately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PbapState {
    /// Initial state. `next_request` will produce the CONNECT.
    Idle,
    /// CONNECT sent, awaiting OK.
    AwaitingConnect,
    /// SETPATH "telecom" sent.
    AwaitingSetPathTelecom,
    /// SETPATH "pb" (relative) sent.
    AwaitingSetPathPb,
    /// GET phonebook sent (initial); awaiting first response.
    AwaitingFirstGet,
    /// GET continuation in progress; awaiting another CONTINUE / OK.
    AwaitingContinuation,
    /// DISCONNECT sent; awaiting final OK before the caller tears the
    /// RFCOMM channel down.
    AwaitingDisconnect,
    /// Terminal: all contacts collected, ready for the caller to take.
    Done,
    /// Terminal failure. The caller can read `reason` and surface it.
    Failed(String),
}

#[derive(Debug)]
pub struct PbapPceSession {
    state: PbapState,
    /// Connection-Id assigned by the PSE in the CONNECT response.
    /// Required on every subsequent request after CONNECT.
    connection_id: Option<u32>,
    /// Reassembles Body chunks across CONTINUE responses.
    body: BodyAssembler,
    /// Parsed contacts, populated when the body is complete.
    contacts: Vec<Contact>,
    /// Whether we've already sent the SETPATH-up that resets to root
    /// before traversing into telecom/pb. Spec recommends starting
    /// from the root so behaviour is deterministic.
    rooted: bool,
    /// Set when the PSE confirms Single Response Mode in any response.
    /// Once true, the server streams body chunks without waiting for
    /// our GET continuations — sending a GET continuation while SRM is
    /// active confuses Bluedroid's PSE on Pixel: it sends the first
    /// chunk fine, accepts our (spec-illegal under SRM) GET continuation,
    /// starts the next chunk, then stalls partway — see
    /// `project_pbap_srm_pse` memory note.
    srm_active: bool,
}

impl PbapPceSession {
    pub fn new() -> Self {
        Self {
            state: PbapState::Idle,
            connection_id: None,
            body: BodyAssembler::new(),
            contacts: Vec::new(),
            rooted: false,
            srm_active: false,
        }
    }

    pub fn state(&self) -> &PbapState {
        &self.state
    }

    pub fn contacts(&self) -> &[Contact] {
        &self.contacts
    }

    pub fn into_contacts(self) -> Vec<Contact> {
        self.contacts
    }

    /// Produce the next outbound OBEX request, advancing internal
    /// state. Returns `None` if we're in a terminal state (Done /
    /// Failed) or already waiting on a response.
    pub fn next_request(&mut self) -> Option<Vec<u8>> {
        match &self.state {
            PbapState::Idle => {
                self.state = PbapState::AwaitingConnect;
                Some(build_connect_request(
                    Some(&PBAP_TARGET_UUID),
                    DEFAULT_MAX_PACKET_LENGTH,
                ))
            }
            // The remaining transitions are response-driven, not
            // tick-driven; `handle_response` advances state and queues
            // its own next request. Returning None here keeps the
            // caller from spamming the wire.
            _ => None,
        }
    }

    /// Feed the parsed response packet back in. Returns the next
    /// outbound packet to send, if any. Wraps each step's transition
    /// in a Result so a malformed/refused response collapses cleanly
    /// into `PbapState::Failed`.
    pub fn handle_response(&mut self, response: &Packet) -> Option<Vec<u8>> {
        match self.advance(response) {
            Ok(next) => next,
            Err(reason) => {
                self.state = PbapState::Failed(reason);
                None
            }
        }
    }

    fn advance(&mut self, response: &Packet) -> Result<Option<Vec<u8>>, String> {
        match self.state.clone() {
            PbapState::AwaitingConnect => {
                require_response_code(response, &[RSP_OK], "CONNECT")?;
                let conn = response
                    .header(HDR_CONNECTION_ID)
                    .and_then(|h| match h {
                        Header::Quad { value, .. } => Some(*value),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        "PBAP PSE accepted CONNECT but omitted Connection-Id".to_string()
                    })?;
                self.connection_id = Some(conn);
                // Per spec, root-relative traversal: SETPATH backup
                // until at root is one option, but PBAP servers all
                // start sessions at root, so we just dive into telecom.
                self.rooted = true;
                self.state = PbapState::AwaitingSetPathTelecom;
                Ok(Some(build_setpath(conn, Some("telecom"), 0)))
            }
            PbapState::AwaitingSetPathTelecom => {
                require_response_code(response, &[RSP_OK], "SETPATH telecom")?;
                let conn = self.required_connection_id()?;
                self.state = PbapState::AwaitingSetPathPb;
                Ok(Some(build_setpath(conn, Some("pb"), 0)))
            }
            PbapState::AwaitingSetPathPb => {
                require_response_code(response, &[RSP_OK], "SETPATH pb")?;
                let conn = self.required_connection_id()?;
                self.state = PbapState::AwaitingFirstGet;
                let extra_headers = vec![
                    Header::name("telecom/pb.vcf"),
                    Header::obex_type(TYPE_PHONEBOOK),
                    // Request Single Response Mode. Pixel/Bluedroid's
                    // PSE will confirm with SRM=0x01 in its first 0x90
                    // response and then stream subsequent body chunks
                    // back-to-back without expecting a GET continuation
                    // from us. We stop sending continuations once
                    // `srm_active` flips true.
                    Header::Byte {
                        id: obex::HDR_SRM,
                        value: 0x01,
                    },
                    // App parameters block: see §5.1.4. Empty here
                    // means "default filter, no max-count" — phones
                    // interpret as "send everything". Real callers
                    // can splice in a max-list-count later without
                    // needing a state-machine change.
                ];
                Ok(Some(build_get_request(conn, extra_headers)))
            }
            PbapState::AwaitingFirstGet | PbapState::AwaitingContinuation => {
                let opcode = response.opcode;
                self.observe_srm(response);
                if opcode == RSP_CONTINUE {
                    self.body.push(response).map_err(|e| e.to_string())?;
                    let conn = self.required_connection_id()?;
                    self.state = PbapState::AwaitingContinuation;
                    if self.srm_active {
                        // Server is streaming under SRM — wait for the
                        // next chunk without sending a continuation.
                        Ok(None)
                    } else {
                        Ok(Some(build_get_continuation(conn)))
                    }
                } else if opcode == RSP_OK {
                    self.body.push(response).map_err(|e| e.to_string())?;
                    if !self.body.is_complete() {
                        // OK without EndOfBody on a multi-packet GET
                        // is a server bug; phones don't do this in
                        // practice but the spec is silent so we surface
                        // it as a clean failure rather than hanging.
                        return Err("PBAP server sent OK without EndOfBody".to_string());
                    }
                    // Decode the assembled vCard stream. PBAP §3.1.1
                    // mandates UTF-8 but in practice some phones emit
                    // stray non-UTF-8 bytes inside otherwise-valid
                    // cards (e.g. legacy contacts imported from older
                    // address books). Using from_utf8_lossy keeps the
                    // good cards parsing — replacement chars that land
                    // inside a TEL or N field will make that one card
                    // fail vCard validation and be dropped silently,
                    // which is what we want.
                    let raw = self.body.bytes();
                    let stream = String::from_utf8_lossy(raw);
                    self.contacts = parse_vcards(&stream);
                    let conn = self.required_connection_id()?;
                    self.state = PbapState::AwaitingDisconnect;
                    Ok(Some(build_disconnect(Some(conn))))
                } else {
                    Err(format!(
                        "PBAP GET phonebook: unexpected response 0x{:02x}",
                        opcode
                    ))
                }
            }
            PbapState::AwaitingDisconnect => {
                // Whether DISCONNECT succeeds or fails, we have what we
                // came for — but log a refusal so a misbehaving PSE
                // gets noticed.
                if response.opcode != RSP_OK {
                    eprintln!(
                        "[PBAP] DISCONNECT returned 0x{:02x} — proceeding as Done anyway",
                        response.opcode
                    );
                }
                self.state = PbapState::Done;
                Ok(None)
            }
            PbapState::Idle => Err("PBAP response received while Idle".to_string()),
            PbapState::Done | PbapState::Failed(_) => {
                Err("PBAP response received after session terminated".to_string())
            }
        }
    }

    fn required_connection_id(&self) -> Result<u32, String> {
        self.connection_id
            .ok_or_else(|| "PBAP session has no Connection-Id".to_string())
    }

    /// Inspect a response for the SRM=enable header. The PSE may add
    /// it to any reply during the GET phase; once seen we latch it on
    /// for the rest of the session. (SRM disable is not modeled — we
    /// haven't seen any PSE flip it back mid-stream.)
    fn observe_srm(&mut self, response: &Packet) {
        if self.srm_active {
            return;
        }
        if let Some(Header::Byte { value, .. }) = response.header(obex::HDR_SRM) {
            if *value == 0x01 {
                self.srm_active = true;
                eprintln!(
                    "[AokieRadio] PBAP SRM=enable confirmed by PSE — pausing GET continuations"
                );
            }
        }
    }
}

impl Default for PbapPceSession {
    fn default() -> Self {
        Self::new()
    }
}

fn require_response_code(packet: &Packet, expected: &[u8], context: &str) -> Result<(), String> {
    if expected.contains(&packet.opcode) {
        Ok(())
    } else {
        Err(format!(
            "PBAP {}: server returned 0x{:02x} (expected one of {:?})",
            context, packet.opcode, expected
        ))
    }
}

// =============================================================================
// Tests.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aokie_radio::obex::{
        build_connect_response, FixedPayload, HDR_NAME, HDR_TARGET, HDR_TYPE, OP_CONNECT,
        OP_DISCONNECT, OP_GET_FINAL, OP_SETPATH, RSP_FORBIDDEN, RSP_NOT_FOUND,
    };

    /// Round-trip helper: encode an outbound packet, parse it back
    /// (with the right FixedPayload kind), assert structural facts.
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
        // Decode what the PSE would have sent: opcode, version/flags/
        // max + Connection-Id + Who. We construct the byte stream via
        // the helper to make sure we mirror real wire format.
        let bytes = build_connect_response(
            RSP_OK,
            DEFAULT_MAX_PACKET_LENGTH,
            Some(connection_id),
            Some(PBAP_TARGET_UUID.to_vec()),
        );
        Packet::parse(&bytes, FixedPayload::Connect).expect("CONNECT response must round-trip")
    }

    #[test]
    fn first_request_is_connect_with_pbap_target() {
        let mut session = PbapPceSession::new();
        let bytes = session.next_request().expect("first request");
        assert_eq!(*session.state(), PbapState::AwaitingConnect);

        let parsed = parse_outbound(&bytes, FixedPayload::Connect);
        assert_eq!(parsed.opcode, OP_CONNECT);
        let target = parsed.header(HDR_TARGET).expect("Target header required");
        match target {
            Header::ByteSeq { value, .. } => assert_eq!(value, &PBAP_TARGET_UUID),
            _ => panic!("Target must be byte-seq"),
        }
    }

    #[test]
    fn happy_path_through_disconnect_yields_contacts() {
        let mut session = PbapPceSession::new();
        let _connect = session.next_request().expect("CONNECT");

        // 1) CONNECT response → expect SETPATH telecom.
        let next = session
            .handle_response(&ok_connect_response(0xA5A5))
            .expect("after CONNECT we send SETPATH");
        let parsed = parse_outbound(&next, FixedPayload::SetPath);
        assert_eq!(parsed.opcode, OP_SETPATH);
        assert_eq!(*session.state(), PbapState::AwaitingSetPathTelecom);
        let name = parsed.header(HDR_NAME).expect("SETPATH must carry Name");
        match name {
            Header::Unicode { value, .. } => assert_eq!(value, "telecom"),
            _ => panic!("Name must be Unicode"),
        }

        // 2) SETPATH telecom OK → expect SETPATH pb.
        let next = session
            .handle_response(&ok_response(0xA5A5, vec![]))
            .expect("SETPATH pb");
        let parsed = parse_outbound(&next, FixedPayload::SetPath);
        assert_eq!(*session.state(), PbapState::AwaitingSetPathPb);
        let name = parsed.header(HDR_NAME).unwrap();
        match name {
            Header::Unicode { value, .. } => assert_eq!(value, "pb"),
            _ => panic!("Name must be Unicode"),
        }

        // 3) SETPATH pb OK → expect GET phonebook.
        let next = session
            .handle_response(&ok_response(0xA5A5, vec![]))
            .expect("GET phonebook");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_GET_FINAL);
        assert_eq!(*session.state(), PbapState::AwaitingFirstGet);
        match parsed.header(HDR_TYPE).unwrap() {
            Header::ByteSeq { value, .. } => assert_eq!(value, b"x-bt/phonebook\0"),
            _ => panic!("Type must be byte-seq"),
        }

        // 4) PSE replies CONTINUE + Body chunk; we send another GET continuation.
        let body_chunk =
            b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:Alice\r\nTEL:+11111111111\r\nEND:VCARD\r\n".to_vec();
        let cont1 = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0xA5A5),
                Header::body(body_chunk.clone()),
            ],
        };
        let next = session.handle_response(&cont1).expect("GET continuation");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_GET_FINAL);
        assert_eq!(*session.state(), PbapState::AwaitingContinuation);

        // 5) Final OK + EndOfBody chunk → DISCONNECT goes out, contacts populated.
        let body_eob =
            b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:Bob\r\nTEL:+22222222222\r\nEND:VCARD\r\n".to_vec();
        let final_ok = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![Header::connection_id(0xA5A5), Header::end_of_body(body_eob)],
        };
        let next = session.handle_response(&final_ok).expect("DISCONNECT");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_DISCONNECT);
        assert_eq!(*session.state(), PbapState::AwaitingDisconnect);
        assert_eq!(session.contacts().len(), 2);
        assert_eq!(session.contacts()[0].display_name, "Alice");
        assert_eq!(session.contacts()[0].phone_numbers, vec!["+11111111111"]);
        assert_eq!(session.contacts()[1].display_name, "Bob");

        // 6) DISCONNECT OK → terminal Done.
        let final_response = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![],
        };
        let next = session.handle_response(&final_response);
        assert!(next.is_none());
        assert_eq!(*session.state(), PbapState::Done);
        assert_eq!(session.into_contacts().len(), 2);
    }

    #[test]
    fn srm_enable_in_response_pauses_get_continuations() {
        // Pixel/Bluedroid PSE adds SRM=enable to its first GET response
        // and then streams body chunks back-to-back. If we keep sending
        // GET continuations after that, the server stalls partway
        // through the next OBEX packet (observed: phone delivers the
        // first 8192B chunk fine, accepts our continuation, ships ~3KB
        // of the next chunk, then goes silent forever).
        let mut session = PbapPceSession::new();
        // Walk to AwaitingFirstGet.
        let _ = session.next_request().unwrap();
        let _ = session.handle_response(&ok_connect_response(0xA5A5));
        let _ = session.handle_response(&ok_response(0xA5A5, vec![]));
        let _ = session.handle_response(&ok_response(0xA5A5, vec![]));
        assert_eq!(*session.state(), PbapState::AwaitingFirstGet);

        // First CONTINUE: SRM=enable + body chunk → no outbound.
        let cont_with_srm = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0xA5A5),
                Header::Byte {
                    id: obex::HDR_SRM,
                    value: 0x01,
                },
                Header::body(
                    b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:A\r\nTEL:+1\r\nEND:VCARD\r\n".to_vec(),
                ),
            ],
        };
        let next = session.handle_response(&cont_with_srm);
        assert!(
            next.is_none(),
            "SRM=enable means we wait for the next chunk silently"
        );
        assert_eq!(*session.state(), PbapState::AwaitingContinuation);

        // Second CONTINUE: still no outbound (server keeps streaming).
        let cont2 = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0xA5A5),
                Header::body(
                    b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:B\r\nTEL:+2\r\nEND:VCARD\r\n".to_vec(),
                ),
            ],
        };
        let next = session.handle_response(&cont2);
        assert!(next.is_none(), "still under SRM");

        // Final OK with EndOfBody: we send DISCONNECT as usual.
        let final_ok = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0xA5A5),
                Header::end_of_body(
                    b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:C\r\nTEL:+3\r\nEND:VCARD\r\n".to_vec(),
                ),
            ],
        };
        let next = session.handle_response(&final_ok).unwrap();
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_DISCONNECT);
        assert_eq!(session.contacts().len(), 3);
    }

    #[test]
    fn no_srm_in_response_still_sends_get_continuations() {
        // Inverse of the SRM test — confirm the legacy non-SRM path
        // still works for servers that don't support / don't enable
        // SRM.
        let mut session = PbapPceSession::new();
        let _ = session.next_request().unwrap();
        let _ = session.handle_response(&ok_connect_response(0xA5A5));
        let _ = session.handle_response(&ok_response(0xA5A5, vec![]));
        let _ = session.handle_response(&ok_response(0xA5A5, vec![]));

        let cont_no_srm = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(0xA5A5),
                Header::body(
                    b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:A\r\nTEL:+1\r\nEND:VCARD\r\n".to_vec(),
                ),
            ],
        };
        let next = session
            .handle_response(&cont_no_srm)
            .expect("non-SRM CONTINUE → GET continuation");
        let parsed = parse_outbound(&next, FixedPayload::None);
        assert_eq!(parsed.opcode, OP_GET_FINAL);
    }

    #[test]
    fn connect_failure_collapses_to_failed() {
        let mut session = PbapPceSession::new();
        let _ = session.next_request();

        // Pretend the PSE refuses CONNECT with FORBIDDEN — happens on
        // Android when the user denies the bond's "Allow contacts
        // access" prompt.
        let denied = Packet {
            opcode: RSP_FORBIDDEN,
            fixed_payload: vec![0x10, 0x00, 0x40, 0x00],
            headers: vec![],
        };
        let next = session.handle_response(&denied);
        assert!(next.is_none());
        match session.state() {
            PbapState::Failed(reason) => {
                assert!(reason.contains("FORBIDDEN") || reason.contains("0xc3"))
            }
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn connect_response_without_connection_id_collapses_to_failed() {
        // A spec-compliant PSE always sends Connection-Id in CONNECT
        // OK; if it doesn't we have nothing to put on subsequent
        // requests. Surface this as a clean failure.
        let mut session = PbapPceSession::new();
        let _ = session.next_request();
        let no_conn = Packet {
            opcode: RSP_OK,
            fixed_payload: vec![0x10, 0x00, 0x40, 0x00],
            headers: vec![Header::who(PBAP_TARGET_UUID.to_vec())],
        };
        let next = session.handle_response(&no_conn);
        assert!(next.is_none());
        match session.state() {
            PbapState::Failed(reason) => assert!(reason.contains("Connection-Id")),
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn setpath_pb_not_found_collapses_to_failed() {
        // SETPATH "telecom/pb" can return NOT_FOUND on phones with
        // empty phonebooks. We surface that loudly so the caller
        // doesn't loop trying.
        let mut session = PbapPceSession::new();
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![])); // telecom OK

        let pb_missing = Packet {
            opcode: RSP_NOT_FOUND,
            fixed_payload: Vec::new(),
            headers: vec![],
        };
        let next = session.handle_response(&pb_missing);
        assert!(next.is_none());
        assert!(matches!(session.state(), PbapState::Failed(_)));
    }

    #[test]
    fn invalid_utf8_in_body_does_not_drop_the_whole_batch() {
        // PBAP §3.1.1 mandates UTF-8 but real phones occasionally
        // emit stray non-UTF-8 bytes inside otherwise-valid streams
        // (legacy contacts imported from older address books with
        // mis-tagged charsets). One bad byte sequence should NOT take
        // down the entire phonebook fetch — the offending card gets
        // dropped silently by the vCard parser and the rest survive.
        let mut session = PbapPceSession::new();
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![]));
        let _ = session.handle_response(&ok_response(7, vec![]));

        // Mixed body: one good card, one card with a lone continuation
        // byte (0xa0) inside the FN field, and one more good card.
        let mut body = Vec::new();
        body.extend_from_slice(
            b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:Alice\r\nTEL:+11111111111\r\nEND:VCARD\r\n",
        );
        body.extend_from_slice(b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:");
        body.extend_from_slice(&[0xff, 0xa0, 0xa0]);
        body.extend_from_slice(b"\r\nTEL:+99999999999\r\nEND:VCARD\r\n");
        body.extend_from_slice(
            b"BEGIN:VCARD\r\nVERSION:2.1\r\nFN:Carol\r\nTEL:+33333333333\r\nEND:VCARD\r\n",
        );

        let final_ok = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![Header::connection_id(7), Header::end_of_body(body)],
        };
        let next = session.handle_response(&final_ok).expect("DISCONNECT");
        assert_eq!(
            parse_outbound(&next, FixedPayload::None).opcode,
            OP_DISCONNECT
        );
        assert_eq!(*session.state(), PbapState::AwaitingDisconnect);

        // The two well-formed cards must come through. The middle one
        // had its FN replaced by U+FFFD via from_utf8_lossy; the vCard
        // parser is permissive so it may still produce a contact, but
        // the contract this test pins is "we don't lose the good
        // cards" — Alice and Carol must be present.
        let names: Vec<&str> = session
            .contacts()
            .iter()
            .map(|c| c.display_name.as_str())
            .collect();
        assert!(
            names.contains(&"Alice"),
            "Alice must survive, got {:?}",
            names
        );
        assert!(
            names.contains(&"Carol"),
            "Carol must survive, got {:?}",
            names
        );
    }

    #[test]
    fn empty_phonebook_eob_yields_zero_contacts_done() {
        // Test PSEs with an actually-empty pb.vcf — they still reply
        // OK + EndOfBody, just with empty body. The session should
        // still end cleanly with zero contacts.
        let mut session = PbapPceSession::new();
        let _ = session.next_request();
        let _ = session.handle_response(&ok_connect_response(7));
        let _ = session.handle_response(&ok_response(7, vec![]));
        let _ = session.handle_response(&ok_response(7, vec![]));
        let final_ok = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![Header::connection_id(7), Header::end_of_body(vec![])],
        };
        let next = session.handle_response(&final_ok);
        assert_eq!(
            parse_outbound(&next.unwrap(), FixedPayload::None).opcode,
            OP_DISCONNECT
        );
        assert!(session.contacts().is_empty());

        // DISCONNECT OK → Done.
        let _ = session.handle_response(&Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![],
        });
        assert_eq!(*session.state(), PbapState::Done);
    }

    #[test]
    fn response_after_done_returns_failed() {
        let mut session = PbapPceSession {
            state: PbapState::Done,
            connection_id: Some(7),
            body: BodyAssembler::new(),
            contacts: vec![],
            rooted: true,
            srm_active: false,
        };
        let _ = session.handle_response(&Packet {
            opcode: RSP_OK,
            fixed_payload: vec![],
            headers: vec![],
        });
        assert!(matches!(session.state(), PbapState::Failed(_)));
    }

    #[test]
    fn next_request_after_terminal_returns_none() {
        let mut session = PbapPceSession::new();
        session.state = PbapState::Done;
        assert!(session.next_request().is_none());
        session.state = PbapState::Failed("test".into());
        assert!(session.next_request().is_none());
    }

    #[test]
    fn pbap_target_uuid_matches_spec() {
        // Sanity check — we lifted these bytes from the PBAP spec
        // (§4.1.1). Pin the constant so a future "let's tidy up the
        // hex" change can't silently change what we negotiate against.
        assert_eq!(
            PBAP_TARGET_UUID,
            [
                0x79, 0x61, 0x35, 0xf0, 0xf0, 0xc5, 0x11, 0xd8, 0x09, 0x66, 0x08, 0x00, 0x20, 0x0c,
                0x9a, 0x66
            ]
        );
    }
}
