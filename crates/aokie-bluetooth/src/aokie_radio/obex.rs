//! OBEX (Object Exchange Protocol) wire codec.
//!
//! Phase 1a of the MAP/PBAP plan. This module is **pure protocol** —
//! no I/O, no Tauri state. It encodes outbound OBEX packets, decodes
//! inbound ones, and assembles multi-packet GET bodies. Higher-level
//! profiles (PBAP, MAP) use it via thin state machines that drive the
//! request/response sequence.
//!
//! Spec reference: OBEX 1.5 + IrDA OBEX. Packet shape:
//!
//! ```text
//! [opcode : 1B][packet_length : 2B BE][fixed payload? : N B][headers : variable]
//! ```
//!
//! Headers are tag-length-value where the tag's top two bits encode the
//! value type:
//!   - `00` → Unicode (UTF-16BE, null-terminated, length-prefixed)
//!   - `01` → byte sequence (length-prefixed)
//!   - `10` → 1-byte value (no length)
//!   - `11` → 4-byte value (no length)
//!
//! Multi-packet GET: the server replies CONTINUE (0x90) + Body chunks
//! until OK (0xA0) + EndOfBody. Use `BodyAssembler` to glue the chunks.

#![allow(dead_code)] // Phase 1a — runtime integration follows in 1d/1e.

// =============================================================================
// Opcodes (top bit = final-packet flag for multi-packet requests).
// =============================================================================

pub const OP_CONNECT: u8 = 0x80; // always final
pub const OP_DISCONNECT: u8 = 0x81; // always final
pub const OP_PUT: u8 = 0x02;
pub const OP_PUT_FINAL: u8 = 0x82;
pub const OP_GET: u8 = 0x03;
pub const OP_GET_FINAL: u8 = 0x83;
pub const OP_SETPATH: u8 = 0x85; // always final
pub const OP_ABORT: u8 = 0xFF; // always final

/// Response opcodes. Spec says responses always carry the final bit
/// (top bit = 1); the value below is the post-mask response code.
pub const RSP_CONTINUE: u8 = 0x90;
pub const RSP_OK: u8 = 0xA0;
pub const RSP_BAD_REQUEST: u8 = 0xC0;
pub const RSP_UNAUTHORIZED: u8 = 0xC1;
pub const RSP_FORBIDDEN: u8 = 0xC3;
pub const RSP_NOT_FOUND: u8 = 0xC4;
pub const RSP_NOT_ACCEPTABLE: u8 = 0xC6;
pub const RSP_NOT_IMPLEMENTED: u8 = 0xD1;
pub const RSP_SERVICE_UNAVAILABLE: u8 = 0xD3;

// =============================================================================
// Header IDs.
// =============================================================================

pub const HDR_COUNT: u8 = 0xC0; // 4-byte: object count
pub const HDR_NAME: u8 = 0x01; // Unicode: object name
pub const HDR_TYPE: u8 = 0x42; // byte seq: MIME-like type, null-terminated ASCII
pub const HDR_LENGTH: u8 = 0xC3; // 4-byte: object length
pub const HDR_TIME: u8 = 0x44; // byte seq: ISO 8601 time
pub const HDR_DESCRIPTION: u8 = 0x05; // Unicode
pub const HDR_TARGET: u8 = 0x46; // byte seq: target service UUID
pub const HDR_HTTP: u8 = 0x47; // byte seq
pub const HDR_BODY: u8 = 0x48; // byte seq: chunk of body data
pub const HDR_END_OF_BODY: u8 = 0x49; // byte seq: final chunk
pub const HDR_WHO: u8 = 0x4A; // byte seq: peer service UUID
pub const HDR_CONNECTION_ID: u8 = 0xCB; // 4-byte: session id
pub const HDR_APP_PARAMETERS: u8 = 0x4C; // byte seq: profile-specific tag-length-value
pub const HDR_AUTH_CHALLENGE: u8 = 0x4D; // byte seq
pub const HDR_AUTH_RESPONSE: u8 = 0x4E; // byte seq
pub const HDR_OBJECT_CLASS: u8 = 0x51; // byte seq
pub const HDR_SRM: u8 = 0x97; // 1-byte: Single Response Mode flag
pub const HDR_SRMP: u8 = 0x98; // 1-byte: SRM parameter

// =============================================================================
// Header type discrimination.
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderKind {
    Unicode,
    ByteSeq,
    Byte,
    Quad,
}

/// Decode the type encoded in a header ID's top two bits.
pub fn header_kind(id: u8) -> HeaderKind {
    match id & 0xC0 {
        0x00 => HeaderKind::Unicode,
        0x40 => HeaderKind::ByteSeq,
        0x80 => HeaderKind::Byte,
        0xC0 => HeaderKind::Quad,
        _ => unreachable!(),
    }
}

// =============================================================================
// Header — typed enum + encoder/decoder.
// =============================================================================

/// A single OBEX header. The variant is determined by the header ID's
/// type bits; passing an ID whose type doesn't match the variant is a
/// programming error and the encoder will produce wire-invalid bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Header {
    Unicode { id: u8, value: String },
    ByteSeq { id: u8, value: Vec<u8> },
    Byte { id: u8, value: u8 },
    Quad { id: u8, value: u32 },
}

impl Header {
    pub fn id(&self) -> u8 {
        match self {
            Header::Unicode { id, .. }
            | Header::ByteSeq { id, .. }
            | Header::Byte { id, .. }
            | Header::Quad { id, .. } => *id,
        }
    }

    /// Encoded byte length of the header on the wire.
    pub fn encoded_len(&self) -> usize {
        match self {
            // 1B id + 2B length + N codepoints * 2B each + 2B null
            Header::Unicode { value, .. } => 1 + 2 + value.encode_utf16().count() * 2 + 2,
            // 1B id + 2B length + payload
            Header::ByteSeq { value, .. } => 1 + 2 + value.len(),
            Header::Byte { .. } => 1 + 1,
            Header::Quad { .. } => 1 + 4,
        }
    }

    /// Encode the header into `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Header::Unicode { id, value } => {
                debug_assert_eq!(header_kind(*id), HeaderKind::Unicode);
                out.push(*id);
                let utf16: Vec<u16> = value.encode_utf16().collect();
                // OBEX Unicode headers are null-terminated UTF-16BE.
                let total_len = (1 + 2 + utf16.len() * 2 + 2) as u16;
                out.extend_from_slice(&total_len.to_be_bytes());
                for u in utf16 {
                    out.extend_from_slice(&u.to_be_bytes());
                }
                out.extend_from_slice(&[0, 0]); // null terminator
            }
            Header::ByteSeq { id, value } => {
                debug_assert_eq!(header_kind(*id), HeaderKind::ByteSeq);
                out.push(*id);
                let total_len = (1 + 2 + value.len()) as u16;
                out.extend_from_slice(&total_len.to_be_bytes());
                out.extend_from_slice(value);
            }
            Header::Byte { id, value } => {
                debug_assert_eq!(header_kind(*id), HeaderKind::Byte);
                out.push(*id);
                out.push(*value);
            }
            Header::Quad { id, value } => {
                debug_assert_eq!(header_kind(*id), HeaderKind::Quad);
                out.push(*id);
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
    }

    /// Parse a single header from the start of `input`. Returns the
    /// header and the number of bytes consumed.
    pub fn parse(input: &[u8]) -> Result<(Header, usize), String> {
        let id = *input
            .first()
            .ok_or_else(|| "OBEX header is empty".to_string())?;
        match header_kind(id) {
            HeaderKind::Unicode => {
                require_len(input, 3, "OBEX Unicode header length")?;
                let total_len = u16::from_be_bytes([input[1], input[2]]) as usize;
                if total_len < 3 || total_len > input.len() {
                    return Err(format!(
                        "OBEX Unicode header total_len {} doesn't fit in {} bytes",
                        total_len,
                        input.len()
                    ));
                }
                let body = &input[3..total_len];
                if body.len() % 2 != 0 {
                    return Err("OBEX Unicode header body length not even".to_string());
                }
                // Strip the trailing null terminator if present (0x0000).
                let body = if body.len() >= 2 && body[body.len() - 2..] == [0, 0] {
                    &body[..body.len() - 2]
                } else {
                    body
                };
                let mut units = Vec::with_capacity(body.len() / 2);
                for chunk in body.chunks_exact(2) {
                    units.push(u16::from_be_bytes([chunk[0], chunk[1]]));
                }
                let value = String::from_utf16(&units)
                    .map_err(|_| "OBEX Unicode header is not valid UTF-16".to_string())?;
                Ok((Header::Unicode { id, value }, total_len))
            }
            HeaderKind::ByteSeq => {
                require_len(input, 3, "OBEX ByteSeq header length")?;
                let total_len = u16::from_be_bytes([input[1], input[2]]) as usize;
                if total_len < 3 || total_len > input.len() {
                    return Err(format!(
                        "OBEX ByteSeq header total_len {} doesn't fit in {} bytes",
                        total_len,
                        input.len()
                    ));
                }
                Ok((
                    Header::ByteSeq {
                        id,
                        value: input[3..total_len].to_vec(),
                    },
                    total_len,
                ))
            }
            HeaderKind::Byte => {
                require_len(input, 2, "OBEX Byte header")?;
                Ok((
                    Header::Byte {
                        id,
                        value: input[1],
                    },
                    2,
                ))
            }
            HeaderKind::Quad => {
                require_len(input, 5, "OBEX Quad header")?;
                let value = u32::from_be_bytes([input[1], input[2], input[3], input[4]]);
                Ok((Header::Quad { id, value }, 5))
            }
        }
    }

    pub fn name(value: impl Into<String>) -> Self {
        Header::Unicode {
            id: HDR_NAME,
            value: value.into(),
        }
    }

    /// Type header value: ASCII string, null-terminated as the OBEX
    /// convention requires (most peers treat the trailing null as part
    /// of the payload).
    pub fn obex_type(value: &str) -> Self {
        let mut bytes = value.as_bytes().to_vec();
        if !bytes.ends_with(&[0]) {
            bytes.push(0);
        }
        Header::ByteSeq {
            id: HDR_TYPE,
            value: bytes,
        }
    }

    pub fn target(uuid: Vec<u8>) -> Self {
        Header::ByteSeq {
            id: HDR_TARGET,
            value: uuid,
        }
    }

    pub fn who(uuid: Vec<u8>) -> Self {
        Header::ByteSeq {
            id: HDR_WHO,
            value: uuid,
        }
    }

    pub fn connection_id(id: u32) -> Self {
        Header::Quad {
            id: HDR_CONNECTION_ID,
            value: id,
        }
    }

    pub fn app_parameters(bytes: Vec<u8>) -> Self {
        Header::ByteSeq {
            id: HDR_APP_PARAMETERS,
            value: bytes,
        }
    }

    pub fn body(bytes: Vec<u8>) -> Self {
        Header::ByteSeq {
            id: HDR_BODY,
            value: bytes,
        }
    }

    pub fn end_of_body(bytes: Vec<u8>) -> Self {
        Header::ByteSeq {
            id: HDR_END_OF_BODY,
            value: bytes,
        }
    }
}

// =============================================================================
// Packet — the unit of one request or one response.
// =============================================================================

/// Discriminator passed to `Packet::parse` so we know how many
/// "fixed payload" bytes precede the headers. CONNECT carries 4 bytes
/// (version, flags, max_packet_length), SETPATH carries 2 (flags,
/// constants). Everything else has 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedPayload {
    None,
    Connect,
    SetPath,
}

impl FixedPayload {
    pub fn len(self) -> usize {
        match self {
            FixedPayload::None => 0,
            FixedPayload::Connect => 4,
            FixedPayload::SetPath => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub opcode: u8,
    /// Fixed payload that precedes the headers. Empty for most
    /// packets; 4 bytes for CONNECT (version/flags/max_packet_length);
    /// 2 bytes for SETPATH (flags, constants).
    pub fixed_payload: Vec<u8>,
    pub headers: Vec<Header>,
}

impl Packet {
    pub fn encode(&self) -> Vec<u8> {
        let mut headers_bytes = Vec::new();
        for h in &self.headers {
            h.encode(&mut headers_bytes);
        }
        let total = 3 + self.fixed_payload.len() + headers_bytes.len();
        let mut out = Vec::with_capacity(total);
        out.push(self.opcode);
        out.extend_from_slice(&(total as u16).to_be_bytes());
        out.extend_from_slice(&self.fixed_payload);
        out.extend_from_slice(&headers_bytes);
        out
    }

    /// Parse a complete OBEX packet. The caller indicates the expected
    /// fixed-payload shape (None / Connect / SetPath) — for responses,
    /// CONNECT-response carries a fixed payload, all others don't.
    pub fn parse(input: &[u8], fixed: FixedPayload) -> Result<Packet, String> {
        require_len(input, 3, "OBEX packet header")?;
        let opcode = input[0];
        let total = u16::from_be_bytes([input[1], input[2]]) as usize;
        if total < 3 + fixed.len() {
            return Err(format!(
                "OBEX packet length {} too small for opcode 0x{:02x} + fixed-payload {}B",
                total,
                opcode,
                fixed.len()
            ));
        }
        if total > input.len() {
            return Err(format!(
                "OBEX packet length {} exceeds buffer {}",
                total,
                input.len()
            ));
        }
        let fixed_end = 3 + fixed.len();
        let fixed_payload = input[3..fixed_end].to_vec();

        let mut headers = Vec::new();
        let mut offset = fixed_end;
        while offset < total {
            let (h, used) = Header::parse(&input[offset..total])?;
            headers.push(h);
            offset += used;
        }
        Ok(Packet {
            opcode,
            fixed_payload,
            headers,
        })
    }

    pub fn header(&self, id: u8) -> Option<&Header> {
        self.headers.iter().find(|h| h.id() == id)
    }

    /// Convenience: response code (top bit of opcode is the final bit;
    /// strip it for pretty matching against `RSP_*`). For requests this
    /// is meaningless and callers shouldn't use it on requests.
    pub fn response_code(&self) -> u8 {
        self.opcode
    }
}

// =============================================================================
// Builders for the requests PBAP/MAP need.
// =============================================================================

/// Build an OBEX CONNECT request.
///
/// `target_uuid` carries the service UUID for profiles like PBAP/MAP
/// (PBAP target = `796135f0-f0c5-11d8-0966-0800200c9a66`). Pass `None`
/// for a generic OBEX-FTP-style connect.
///
/// `max_packet_length` advertises our preferred per-packet size — peers
/// echo this back as their own preference and the smaller of the two
/// becomes the negotiated MTU.
pub fn build_connect_request(target_uuid: Option<&[u8]>, max_packet_length: u16) -> Vec<u8> {
    let mut headers = Vec::new();
    if let Some(uuid) = target_uuid {
        headers.push(Header::target(uuid.to_vec()));
    }
    Packet {
        opcode: OP_CONNECT,
        fixed_payload: build_connect_fixed(0x10, 0x00, max_packet_length),
        headers,
    }
    .encode()
}

/// Build a CONNECT response packet (server side; we'll need this for
/// MNS later). `max_packet_length` is what we accept inbound;
/// `connection_id` becomes the session handle the peer uses on
/// subsequent requests.
pub fn build_connect_response(
    rsp: u8,
    max_packet_length: u16,
    connection_id: Option<u32>,
    who: Option<Vec<u8>>,
) -> Vec<u8> {
    let mut headers = Vec::new();
    if let Some(id) = connection_id {
        headers.push(Header::connection_id(id));
    }
    if let Some(who_uuid) = who {
        headers.push(Header::who(who_uuid));
    }
    Packet {
        opcode: rsp,
        fixed_payload: build_connect_fixed(0x10, 0x00, max_packet_length),
        headers,
    }
    .encode()
}

fn build_connect_fixed(version: u8, flags: u8, max_packet_length: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.push(version);
    out.push(flags);
    out.extend_from_slice(&max_packet_length.to_be_bytes());
    out
}

pub fn build_disconnect(connection_id: Option<u32>) -> Vec<u8> {
    let mut headers = Vec::new();
    if let Some(id) = connection_id {
        headers.push(Header::connection_id(id));
    }
    Packet {
        opcode: OP_DISCONNECT,
        fixed_payload: Vec::new(),
        headers,
    }
    .encode()
}

/// Build an OBEX GET — always with the final bit set (caller is expected
/// to send subsequent CONTINUE replies in a separate packet, but the
/// initial GET carries all the request headers and is itself a complete
/// request packet per the spec).
pub fn build_get_request(connection_id: u32, additional: Vec<Header>) -> Vec<u8> {
    let mut headers = Vec::with_capacity(1 + additional.len());
    headers.push(Header::connection_id(connection_id));
    headers.extend(additional);
    Packet {
        opcode: OP_GET_FINAL,
        fixed_payload: Vec::new(),
        headers,
    }
    .encode()
}

/// Build a GET continuation — same opcode, only the Connection-ID
/// header. The server uses this as a "give me the next chunk" signal
/// when SRM isn't enabled.
pub fn build_get_continuation(connection_id: u32) -> Vec<u8> {
    Packet {
        opcode: OP_GET_FINAL,
        fixed_payload: Vec::new(),
        headers: vec![Header::connection_id(connection_id)],
    }
    .encode()
}

/// SETPATH flag bits (per OBEX spec):
///   - bit 0: "Backup a level then apply name" (i.e. cd ../<name>)
///   - bit 1: "Don't create folder if it doesn't exist"
pub const SETPATH_FLAG_BACKUP: u8 = 0x01;
pub const SETPATH_FLAG_NOCREATE: u8 = 0x02;

pub fn build_setpath(connection_id: u32, name: Option<&str>, flags: u8) -> Vec<u8> {
    let mut headers = vec![Header::connection_id(connection_id)];
    if let Some(n) = name {
        headers.push(Header::name(n));
    }
    let mut fixed_payload = Vec::with_capacity(2);
    fixed_payload.push(flags);
    fixed_payload.push(0); // constants
    Packet {
        opcode: OP_SETPATH,
        fixed_payload,
        headers,
    }
    .encode()
}

/// Build a single-packet OBEX PUT (final bit set). The caller passes the
/// content (bMessage envelope, vCard, etc.) as a single byte buffer; we
/// emit it as one EndOfBody chunk so the request fits in one packet.
/// MAP/PBAP single-message PUTs are well under the negotiated MTU; if a
/// future caller needs to fragment, add a dedicated multi-packet PUT
/// builder instead of overloading this one.
pub fn build_put_request(
    connection_id: u32,
    additional_headers: Vec<Header>,
    body: Vec<u8>,
) -> Vec<u8> {
    let mut headers = Vec::with_capacity(2 + additional_headers.len());
    headers.push(Header::connection_id(connection_id));
    headers.extend(additional_headers);
    headers.push(Header::end_of_body(body));
    Packet {
        opcode: OP_PUT_FINAL,
        fixed_payload: Vec::new(),
        headers,
    }
    .encode()
}

pub fn build_abort(connection_id: u32) -> Vec<u8> {
    Packet {
        opcode: OP_ABORT,
        fixed_payload: Vec::new(),
        headers: vec![Header::connection_id(connection_id)],
    }
    .encode()
}

// =============================================================================
// Multi-packet body assembly (for GET responses).
// =============================================================================

/// Glues Body / EndOfBody chunks across a sequence of GET responses.
/// The flow:
///
/// ```text
/// → GET (Type=x-bt/phonebook, AppParams)
/// ← CONTINUE + Body(chunk1)
/// → GET continuation
/// ← CONTINUE + Body(chunk2)
/// → GET continuation
/// ← OK + EndOfBody(chunk3)
/// ```
///
/// `BodyAssembler` collects chunk1..chunk3 into a single Vec<u8> and
/// flips `complete = true` when the EndOfBody arrives.
#[derive(Debug, Default)]
pub struct BodyAssembler {
    bytes: Vec<u8>,
    complete: bool,
}

impl BodyAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed in a parsed response packet. Returns true if the body is
    /// now complete (i.e. an EndOfBody arrived).
    pub fn push(&mut self, packet: &Packet) -> Result<bool, String> {
        if self.complete {
            return Err("BodyAssembler: response after EndOfBody".to_string());
        }
        for h in &packet.headers {
            match h {
                Header::ByteSeq { id, value } if *id == HDR_BODY => {
                    self.bytes.extend_from_slice(value);
                }
                Header::ByteSeq { id, value } if *id == HDR_END_OF_BODY => {
                    self.bytes.extend_from_slice(value);
                    self.complete = true;
                }
                _ => {}
            }
        }
        Ok(self.complete)
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

// =============================================================================
// Internal utilities.
// =============================================================================

fn require_len(input: &[u8], min_len: usize, name: &str) -> Result<(), String> {
    if input.len() < min_len {
        return Err(format!(
            "{} is too short: expected at least {} bytes, got {}",
            name,
            min_len,
            input.len()
        ));
    }
    Ok(())
}

// =============================================================================
// Tests.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_kind_decodes_top_two_bits() {
        assert_eq!(header_kind(HDR_NAME), HeaderKind::Unicode); // 0x01 → 00xxxxxx
        assert_eq!(header_kind(HDR_TYPE), HeaderKind::ByteSeq); // 0x42 → 01xxxxxx
        assert_eq!(header_kind(HDR_BODY), HeaderKind::ByteSeq);
        assert_eq!(header_kind(HDR_END_OF_BODY), HeaderKind::ByteSeq);
        assert_eq!(header_kind(HDR_TARGET), HeaderKind::ByteSeq);
        assert_eq!(header_kind(HDR_SRM), HeaderKind::Byte); // 0x97 → 10xxxxxx
        assert_eq!(header_kind(HDR_CONNECTION_ID), HeaderKind::Quad); // 0xCB → 11xxxxxx
        assert_eq!(header_kind(HDR_LENGTH), HeaderKind::Quad);
        assert_eq!(header_kind(HDR_COUNT), HeaderKind::Quad);
    }

    #[test]
    fn header_round_trip_unicode() {
        let h = Header::name("hello");
        let mut bytes = Vec::new();
        h.encode(&mut bytes);
        // 0x01 + 2B len + 5*2B UTF-16BE + 2B null = 1 + 2 + 10 + 2 = 15
        assert_eq!(bytes.len(), 15);
        assert_eq!(bytes[0], HDR_NAME);
        assert_eq!(
            u16::from_be_bytes([bytes[1], bytes[2]]) as usize,
            bytes.len()
        );
        let (parsed, used) = Header::parse(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(
            parsed,
            Header::Unicode {
                id: HDR_NAME,
                value: "hello".into()
            }
        );
    }

    #[test]
    fn header_round_trip_unicode_empty() {
        // OBEX SETPATH "go to root" uses an empty Name header — this
        // must encode as id + length=5 (1 + 2 + 0 + 2 null) + null
        // terminator only. Phones reject anything else.
        let h = Header::name("");
        let mut bytes = Vec::new();
        h.encode(&mut bytes);
        assert_eq!(bytes, [HDR_NAME, 0x00, 0x05, 0x00, 0x00]);
        let (parsed, used) = Header::parse(&bytes).unwrap();
        assert_eq!(used, 5);
        assert_eq!(
            parsed,
            Header::Unicode {
                id: HDR_NAME,
                value: String::new()
            }
        );
    }

    #[test]
    fn header_round_trip_byte_seq() {
        let h = Header::body(b"\x01\x02\x03\x04".to_vec());
        let mut bytes = Vec::new();
        h.encode(&mut bytes);
        assert_eq!(bytes, [HDR_BODY, 0x00, 0x07, 0x01, 0x02, 0x03, 0x04]);
        let (parsed, _) = Header::parse(&bytes).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn header_round_trip_byte_and_quad() {
        let srm = Header::Byte {
            id: HDR_SRM,
            value: 0x01,
        };
        let mut bytes = Vec::new();
        srm.encode(&mut bytes);
        assert_eq!(bytes, [HDR_SRM, 0x01]);
        assert_eq!(Header::parse(&bytes).unwrap(), (srm, 2));

        let conn = Header::connection_id(0xdead_beef);
        let mut bytes = Vec::new();
        conn.encode(&mut bytes);
        assert_eq!(bytes, [HDR_CONNECTION_ID, 0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(Header::parse(&bytes).unwrap(), (conn, 5));
    }

    #[test]
    fn header_unicode_with_non_ascii_round_trips() {
        // Real PBAP entries can have CJK / accented Latin in N/FN.
        let h = Header::name("Süßmüller — 中村");
        let mut bytes = Vec::new();
        h.encode(&mut bytes);
        let (parsed, _) = Header::parse(&bytes).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn parse_rejects_truncated_headers() {
        assert!(Header::parse(&[]).is_err());
        // Unicode header claims length 100 but we only give 3 bytes.
        assert!(Header::parse(&[HDR_NAME, 0x00, 0x64]).is_err());
        // ByteSeq header claims length 100.
        assert!(Header::parse(&[HDR_BODY, 0x00, 0x64, 0x01]).is_err());
        // Byte header missing its value byte.
        assert!(Header::parse(&[HDR_SRM]).is_err());
        // Quad header missing 2 of its 4 value bytes.
        assert!(Header::parse(&[HDR_CONNECTION_ID, 0x01, 0x02]).is_err());
        // Total length under the 3-byte minimum.
        assert!(Header::parse(&[HDR_NAME, 0x00, 0x02]).is_err());
    }

    #[test]
    fn packet_round_trip_get_request() {
        let bytes = build_get_request(0xcafef00d, vec![Header::obex_type("x-bt/phonebook")]);
        let parsed = Packet::parse(&bytes, FixedPayload::None).unwrap();
        assert_eq!(parsed.opcode, OP_GET_FINAL);
        assert!(parsed.fixed_payload.is_empty());
        // First header is Connection-Id, second is Type.
        assert_eq!(
            parsed.header(HDR_CONNECTION_ID),
            Some(&Header::connection_id(0xcafef00d))
        );
        match parsed.header(HDR_TYPE) {
            Some(Header::ByteSeq { value, .. }) => {
                assert_eq!(value, b"x-bt/phonebook\0");
            }
            other => panic!("expected Type byte-seq, got {:?}", other),
        }
    }

    #[test]
    fn packet_round_trip_connect_with_target() {
        // PBAP target UUID, big-endian on the wire.
        let target: &[u8] = &[
            0x79, 0x61, 0x35, 0xf0, 0xf0, 0xc5, 0x11, 0xd8, 0x09, 0x66, 0x08, 0x00, 0x20, 0x0c,
            0x9a, 0x66,
        ];
        let bytes = build_connect_request(Some(target), 0x4000);
        // Front of packet: opcode + length + 4 fixed bytes.
        assert_eq!(bytes[0], OP_CONNECT);
        // version=0x10, flags=0x00, max=0x4000.
        assert_eq!(&bytes[3..7], &[0x10, 0x00, 0x40, 0x00]);

        let parsed = Packet::parse(&bytes, FixedPayload::Connect).unwrap();
        assert_eq!(parsed.opcode, OP_CONNECT);
        assert_eq!(parsed.fixed_payload, [0x10, 0x00, 0x40, 0x00]);
        match parsed.header(HDR_TARGET) {
            Some(Header::ByteSeq { value, .. }) => assert_eq!(value, target),
            other => panic!("expected Target header, got {:?}", other),
        }
    }

    #[test]
    fn packet_round_trip_setpath() {
        let bytes = build_setpath(0x0001, Some("telecom"), 0);
        assert_eq!(bytes[0], OP_SETPATH);
        // 2-byte SETPATH fixed payload after the 3-byte packet header.
        assert_eq!(bytes[3], 0); // flags
        assert_eq!(bytes[4], 0); // constants

        let parsed = Packet::parse(&bytes, FixedPayload::SetPath).unwrap();
        assert_eq!(parsed.fixed_payload, [0, 0]);
        assert!(matches!(
            parsed.header(HDR_NAME),
            Some(Header::Unicode { value, .. }) if value == "telecom"
        ));
    }

    #[test]
    fn packet_round_trip_setpath_backup() {
        // SETPATH "..": no Name header, flags = backup.
        let bytes = build_setpath(0x0042, None, SETPATH_FLAG_BACKUP);
        let parsed = Packet::parse(&bytes, FixedPayload::SetPath).unwrap();
        assert_eq!(parsed.fixed_payload, [SETPATH_FLAG_BACKUP, 0]);
        assert!(parsed.header(HDR_NAME).is_none());
    }

    #[test]
    fn packet_parse_rejects_length_mismatch() {
        // Claim 100B but only give 10.
        let bad = [
            OP_GET_FINAL,
            0x00,
            0x64,
            0xCB,
            0x00,
            0x00,
            0x00,
            0x01,
            0x42,
            0x00,
        ];
        assert!(Packet::parse(&bad, FixedPayload::None).is_err());
    }

    #[test]
    fn packet_parse_rejects_undersize_for_connect_fixed() {
        // CONNECT must include at least 4 fixed bytes; total=5 leaves 0 for fixed.
        let bad = [OP_CONNECT, 0x00, 0x05, 0x10, 0x00];
        assert!(Packet::parse(&bad, FixedPayload::Connect).is_err());
    }

    #[test]
    fn body_assembler_glues_continue_chunks_until_end_of_body() {
        let chunk1 = vec![1, 2, 3, 4];
        let chunk2 = vec![5, 6, 7];
        let chunk3 = vec![8, 9];

        let mut a = BodyAssembler::new();
        let p1 = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![Header::body(chunk1.clone())],
        };
        let p2 = Packet {
            opcode: RSP_CONTINUE,
            fixed_payload: Vec::new(),
            headers: vec![Header::body(chunk2.clone())],
        };
        let p3 = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![Header::end_of_body(chunk3.clone())],
        };

        assert!(!a.push(&p1).unwrap());
        assert!(!a.push(&p2).unwrap());
        assert!(a.push(&p3).unwrap());
        assert!(a.is_complete());

        let mut expected = chunk1.clone();
        expected.extend(chunk2);
        expected.extend(chunk3);
        assert_eq!(a.into_bytes(), expected);
    }

    #[test]
    fn body_assembler_rejects_post_eob_responses() {
        let mut a = BodyAssembler::new();
        let p_eob = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![Header::end_of_body(vec![1, 2])],
        };
        a.push(&p_eob).unwrap();
        // A second push after EndOfBody is a server bug; flag it loudly.
        let p_extra = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![Header::body(vec![3])],
        };
        assert!(a.push(&p_extra).is_err());
    }

    #[test]
    fn body_assembler_ignores_non_body_headers() {
        // Real responses include Length, Connection-Id, Type alongside
        // the Body chunks. BodyAssembler must collect only Body /
        // EndOfBody and ignore the rest, so we can pass full responses
        // straight in without filtering.
        let mut a = BodyAssembler::new();
        let p = Packet {
            opcode: RSP_OK,
            fixed_payload: Vec::new(),
            headers: vec![
                Header::connection_id(7),
                Header::Quad {
                    id: HDR_LENGTH,
                    value: 42,
                },
                Header::end_of_body(vec![0xa, 0xb, 0xc]),
            ],
        };
        a.push(&p).unwrap();
        assert_eq!(a.into_bytes(), vec![0xa, 0xb, 0xc]);
    }

    #[test]
    fn fuzz_packet_parse_does_not_panic_on_random_bytes() {
        // OBEX packets carry a length prefix and Type/Length/Value
        // headers that are easy to disagree about. Random input must
        // surface as Err, never a panic — at runtime we feed this
        // straight from the wire.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x0bec_0bec_0bec_0bec);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 1024) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = Packet::parse(&buf, FixedPayload::None);
            let _ = Packet::parse(&buf, FixedPayload::Connect);
            let _ = Packet::parse(&buf, FixedPayload::SetPath);
        }
    }

    #[test]
    fn fuzz_header_parse_does_not_panic_on_random_bytes() {
        // Header parsing has the most variants (Unicode/byte-seq/u8
        // /u32) and the most length math. Random bytes must always
        // resolve to Ok or Err.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xdead_b0de_dead_b0de);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 256) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = Header::parse(&buf);
        }
    }

    #[test]
    fn fuzz_packet_parse_does_not_panic_on_truncations() {
        // Real wire data: a CONNECT response with a couple of
        // headers. Truncate at every cut point — must never panic.
        let p = Packet {
            opcode: 0xa0,
            fixed_payload: vec![0x10, 0x00, 0xff, 0xff],
            headers: vec![
                Header::connection_id(7),
                Header::who(b"target".to_vec()),
                Header::name("greeting"),
            ],
        };
        let bytes = p.encode();
        for cut in 0..=bytes.len() {
            let _ = Packet::parse(&bytes[..cut], FixedPayload::Connect);
        }
    }
}
