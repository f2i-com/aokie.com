//! bMessage envelope reader and writer.
//!
//! Phase 4c of the SMS auto-reply plan. The bMessage format (MAP §3.1.1)
//! is a vCard-style text envelope wrapping a single message body. A
//! typical SMS bMessage looks like:
//!
//! ```text
//! BEGIN:BMSG
//! VERSION:1.0
//! STATUS:UNREAD
//! TYPE:SMS_GSM
//! FOLDER:telecom/msg/inbox
//! BEGIN:VCARD
//! VERSION:2.1
//! N:Smith;John
//! TEL:+15551234567
//! END:VCARD
//! BEGIN:BENV
//! BEGIN:VCARD
//! VERSION:2.1
//! N:;Receiver
//! TEL:+15559876543
//! END:VCARD
//! BEGIN:BBODY
//! CHARSET:UTF-8
//! LENGTH:42
//! BEGIN:MSG
//! Hello, this is a text message body.
//! END:MSG
//! END:BBODY
//! END:BENV
//! END:BMSG
//! ```
//!
//! For inbound parsing we extract three things: the originator's TEL
//! (sender phone number), the message body (between BEGIN:MSG and
//! END:MSG), and the message type. Anything else is tolerated and
//! ignored.
//!
//! For outbound building we emit the minimal envelope needed for an
//! SMS reply: type=SMS_GSM, folder=telecom/msg/outbox, recipient
//! vCard with TEL, and the body in a BBODY block. The phone fills in
//! the originator from its own SIM details.

#![allow(dead_code)] // Phase 4c — runtime integration follows.

/// Parsed inbound bMessage. Fields are optional because phones vary in
/// what they include — we want a "best effort" view that the runtime
/// can act on rather than a hard schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BMessage {
    /// MAP message type. "SMS_GSM" or "SMS_CDMA" for SMS; we ignore
    /// anything else (EMAIL, MMS, IM) at the runtime layer.
    pub msg_type: Option<String>,
    /// Folder that contained the message, e.g. "telecom/msg/inbox".
    /// Useful for distinguishing inbox vs. drafts vs. sent.
    pub folder: Option<String>,
    /// Originator phone number — the sender of an inbound SMS.
    /// Pulled from the first vCard's TEL field (the originator block
    /// per MAP §3.1.1.2). May be missing if the AG omits the
    /// originator (some phones do this for outbox entries).
    pub sender_addressing: Option<String>,
    /// Originator display name from the vCard's N or FN field. Useful
    /// only as a fallback — we usually resolve names through our own
    /// ContactStore from the sender_addressing.
    pub sender_name: Option<String>,
    /// Plain-text message body. Whatever was between BEGIN:MSG and
    /// END:MSG, with the trailing CRLF stripped.
    pub body: String,
    /// Read status. None unless the AG sent a STATUS line.
    pub read: Option<bool>,
}

/// Parse a bMessage byte buffer. Non-UTF-8 bytes are replaced with
/// U+FFFD via from_utf8_lossy; that keeps a stray legacy-encoding byte
/// from blowing up the whole parse.
pub fn parse(input: &[u8]) -> BMessage {
    let text = String::from_utf8_lossy(input);
    let mut out = BMessage::default();
    let mut depth_envelope = 0usize; // BENV nesting (originator vs recipient block)
    let mut in_msg = false;
    let mut in_vcard_originator = false;
    let mut vcard_seen_count = 0usize;

    let mut body = String::new();

    for raw_line in text.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if in_msg {
            if line.eq_ignore_ascii_case("END:MSG") {
                in_msg = false;
                continue;
            }
            // Preserve newlines in the body but skip the trailing
            // CRLF that immediately precedes END:MSG. We can't know
            // until we see the line, so always append.
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
            continue;
        }

        let upper_line = line.to_ascii_uppercase();
        let upper = upper_line.as_str();
        if upper.starts_with("BEGIN:BENV") {
            depth_envelope += 1;
            // First BENV holds the originator's vCard (per spec); we
            // already captured the originator vCard *before* the BENV
            // begins, so the inner vCards are recipients we ignore.
            continue;
        }
        if upper.starts_with("END:BENV") {
            depth_envelope = depth_envelope.saturating_sub(1);
            continue;
        }
        if upper.starts_with("BEGIN:VCARD") {
            // The first vCard before any BENV is the originator.
            in_vcard_originator = depth_envelope == 0 && vcard_seen_count == 0;
            vcard_seen_count += 1;
            continue;
        }
        if upper.starts_with("END:VCARD") {
            in_vcard_originator = false;
            continue;
        }
        if upper.starts_with("BEGIN:MSG") {
            in_msg = true;
            continue;
        }
        if upper.starts_with("BEGIN:") || upper.starts_with("END:") {
            // BBODY, BMSG, etc. — no per-tag work needed.
            continue;
        }

        // Property lines: PROP[;PARAMS]:VALUE
        let (prop_with_params, value) = match line.find(':') {
            Some(idx) => (&line[..idx], &line[idx + 1..]),
            None => continue,
        };
        let prop = prop_with_params
            .split(';')
            .next()
            .unwrap_or(prop_with_params)
            .trim();
        let value = value.trim();

        match prop.to_ascii_uppercase().as_str() {
            "TYPE" if depth_envelope == 0 && out.msg_type.is_none() => {
                out.msg_type = Some(value.to_string());
            }
            "FOLDER" if depth_envelope == 0 && out.folder.is_none() => {
                out.folder = Some(value.to_string());
            }
            "STATUS" if depth_envelope == 0 && out.read.is_none() => {
                out.read = Some(value.eq_ignore_ascii_case("READ"));
            }
            "TEL" if in_vcard_originator && out.sender_addressing.is_none() => {
                out.sender_addressing = Some(value.to_string());
            }
            "FN" if in_vcard_originator && out.sender_name.is_none() => {
                out.sender_name = Some(value.to_string());
            }
            "N" if in_vcard_originator && out.sender_name.is_none() => {
                // N format: Family;Given;Middle;Prefix;Suffix. Build
                // a "Given Family" display string out of the first two.
                let mut parts = value.split(';');
                let family = parts.next().unwrap_or("").trim();
                let given = parts.next().unwrap_or("").trim();
                let combined = match (given.is_empty(), family.is_empty()) {
                    (false, false) => format!("{} {}", given, family),
                    (true, false) => family.to_string(),
                    (false, true) => given.to_string(),
                    (true, true) => String::new(),
                };
                if !combined.is_empty() {
                    out.sender_name = Some(combined);
                }
            }
            _ => {}
        }
    }

    out.body = body;
    out
}

/// Build an outbound bMessage envelope for a single SMS. The phone
/// fills in the originator from its SIM details so we only emit the
/// recipient and body. Uses CRLF line endings as MAP mandates.
///
/// `recipient_phone` is taken verbatim into the TEL field — the caller
/// is expected to format it the way the AG's carrier wants (most
/// accept E.164 with leading +, but local formats also work).
///
/// `body` is plain UTF-8 text. Multi-line bodies are supported; any
/// embedded "END:MSG" lines are escaped with a leading space (per
/// vCard 2.1 line-folding rules) so we don't accidentally close the
/// MSG block early.
pub fn build_sms_push(recipient_phone: &str, body: &str) -> Vec<u8> {
    let mut out = String::with_capacity(256 + body.len());
    out.push_str("BEGIN:BMSG\r\n");
    out.push_str("VERSION:1.0\r\n");
    out.push_str("STATUS:UNREAD\r\n");
    out.push_str("TYPE:SMS_GSM\r\n");
    out.push_str("FOLDER:telecom/msg/outbox\r\n");
    out.push_str("BEGIN:BENV\r\n");
    out.push_str("BEGIN:VCARD\r\n");
    out.push_str("VERSION:2.1\r\n");
    out.push_str("N:;\r\n");
    out.push_str(&format!("TEL:{}\r\n", recipient_phone));
    out.push_str("END:VCARD\r\n");
    out.push_str("BEGIN:BBODY\r\n");
    out.push_str("CHARSET:UTF-8\r\n");
    let safe_body = escape_msg_body(body);
    out.push_str(&format!("LENGTH:{}\r\n", safe_body.len()));
    out.push_str("BEGIN:MSG\r\n");
    out.push_str(&safe_body);
    if !safe_body.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out.push_str("END:MSG\r\n");
    out.push_str("END:BBODY\r\n");
    out.push_str("END:BENV\r\n");
    out.push_str("END:BMSG\r\n");
    out.into_bytes()
}

/// Build an outbound MMS PushMessage bMessage. Used when the inbound
/// `SmsReceived` reported `msg_type=MMS` — typically Google
/// Messages' RCS-fallback-to-MMS path on Pixel. By echoing MMS back
/// (instead of sending plain SMS_GSM) the reply is dispatched via
/// Pixel's MMS subsystem, which on modern Android transparently
/// upgrades through the RCS path when the recipient is RCS-capable.
/// Sending SMS_GSM as the reply makes it land in a separate legacy
/// thread on the recipient's phone and the operator never sees a
/// reply on their side.
///
/// The MAP envelope is identical to `build_sms_push` except for
/// `TYPE:MMS`; the difference lives inside `BEGIN:MSG` ... `END:MSG`,
/// which carries a small RFC 2822 / MIME message: To header set to
/// the recipient, a date header (Pixel may overwrite it when
/// actually dispatching), and a single `multipart/mixed` body with
/// one `text/plain; charset=utf-8` part containing the reply text.
/// Pixel's `BluetoothMapClient.parseMmsBmessage` parses the inner
/// MIME and feeds it to the MMS dispatcher.
pub fn build_mms_push(recipient_phone: &str, body: &str) -> Vec<u8> {
    build_mms_push_at(recipient_phone, body, chrono::Utc::now())
}

/// Date-injectable variant of `build_mms_push` so tests can assert
/// against a stable byte sequence. Production callers should always
/// use `build_mms_push`, which captures `Utc::now()` at send time.
fn build_mms_push_at(
    recipient_phone: &str,
    body: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<u8> {
    let date_header = now.to_rfc2822();
    // Boundary picked from the second-precision timestamp so it
    // changes per-call but stays deterministic for tests with a
    // fixed `now`. Any string that doesn't appear in the body works
    // — this one is unlikely to collide with reply text.
    let boundary = format!("aokie-mms-{}", now.timestamp());
    let mime_body = build_mms_mime_body(recipient_phone, body, &date_header, &boundary);
    let safe_mime = escape_msg_body(&mime_body);

    let mut out = String::with_capacity(384 + safe_mime.len());
    out.push_str("BEGIN:BMSG\r\n");
    out.push_str("VERSION:1.0\r\n");
    out.push_str("STATUS:UNREAD\r\n");
    out.push_str("TYPE:MMS\r\n");
    out.push_str("FOLDER:telecom/msg/outbox\r\n");
    out.push_str("BEGIN:BENV\r\n");
    out.push_str("BEGIN:VCARD\r\n");
    out.push_str("VERSION:2.1\r\n");
    out.push_str("N:;\r\n");
    out.push_str(&format!("TEL:{}\r\n", recipient_phone));
    out.push_str("END:VCARD\r\n");
    out.push_str("BEGIN:BBODY\r\n");
    out.push_str("CHARSET:UTF-8\r\n");
    out.push_str(&format!("LENGTH:{}\r\n", safe_mime.len()));
    out.push_str("BEGIN:MSG\r\n");
    out.push_str(&safe_mime);
    if !safe_mime.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out.push_str("END:MSG\r\n");
    out.push_str("END:BBODY\r\n");
    out.push_str("END:BENV\r\n");
    out.push_str("END:BMSG\r\n");
    out.into_bytes()
}

/// Compose the RFC 2822 / MIME body that goes inside `BEGIN:MSG` of
/// an MMS bMessage. Single `multipart/mixed` envelope around one
/// `text/plain; charset=utf-8` part — that matches the shape Pixel
/// itself produces for inbound MMS (Google Messages RCS-fallback)
/// and avoids the ambiguity of a non-multipart text body, which
/// some MMS dispatchers reject for missing `Content-Type`.
fn build_mms_mime_body(recipient: &str, body: &str, date_header: &str, boundary: &str) -> String {
    let mut s = String::with_capacity(256 + body.len());
    s.push_str(&format!("Date: {}\r\n", date_header));
    s.push_str("Subject:\r\n");
    s.push_str(&format!("To: {}\r\n", recipient));
    s.push_str("MIME-Version: 1.0\r\n");
    s.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{}\"\r\n",
        boundary
    ));
    s.push_str("\r\n");
    s.push_str(&format!("--{}\r\n", boundary));
    s.push_str("Content-Type: text/plain; charset=\"utf-8\"\r\n");
    s.push_str("Content-Transfer-Encoding: 8BIT\r\n");
    s.push_str("\r\n");
    s.push_str(body);
    if !body.ends_with('\n') {
        s.push_str("\r\n");
    }
    s.push_str(&format!("--{}--\r\n", boundary));
    s
}

/// Prevent a body that happens to contain "END:MSG" from closing the
/// outer MSG block early. vCard 2.1 line folding indents continuation
/// lines with a single space; we use the same trick to neutralize any
/// would-be terminator in the user-supplied body.
fn escape_msg_body(body: &str) -> String {
    let mut escaped = String::with_capacity(body.len());
    for line in body.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.eq_ignore_ascii_case("END:MSG") || line.eq_ignore_ascii_case("BEGIN:MSG") {
            escaped.push(' ');
            escaped.push_str(line);
        } else {
            escaped.push_str(line);
        }
        escaped.push_str("\r\n");
    }
    // Strip the trailing CRLF we just added — we'll re-add one
    // unconditionally in build_sms_push.
    if escaped.ends_with("\r\n") {
        escaped.truncate(escaped.len() - 2);
    }
    escaped
}

// =============================================================================
// Tests.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const INBOUND: &str = "BEGIN:BMSG\r\n\
VERSION:1.0\r\n\
STATUS:UNREAD\r\n\
TYPE:SMS_GSM\r\n\
FOLDER:telecom/msg/inbox\r\n\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
N:Smith;John\r\n\
TEL:+15551234567\r\n\
END:VCARD\r\n\
BEGIN:BENV\r\n\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
N:;Receiver\r\n\
TEL:+15559876543\r\n\
END:VCARD\r\n\
BEGIN:BBODY\r\n\
CHARSET:UTF-8\r\n\
LENGTH:36\r\n\
BEGIN:MSG\r\n\
Hello, this is a text message body.\r\n\
END:MSG\r\n\
END:BBODY\r\n\
END:BENV\r\n\
END:BMSG\r\n";

    #[test]
    fn parses_inbound_bmessage_with_sender_and_body() {
        let parsed = parse(INBOUND.as_bytes());
        assert_eq!(parsed.msg_type.as_deref(), Some("SMS_GSM"));
        assert_eq!(parsed.folder.as_deref(), Some("telecom/msg/inbox"));
        assert_eq!(parsed.sender_addressing.as_deref(), Some("+15551234567"));
        assert_eq!(parsed.sender_name.as_deref(), Some("John Smith"));
        assert_eq!(parsed.read, Some(false)); // STATUS:UNREAD
        assert_eq!(parsed.body, "Hello, this is a text message body.");
    }

    #[test]
    fn parses_multiline_body() {
        let raw = "BEGIN:BMSG\r\nTYPE:SMS_GSM\r\nFOLDER:telecom/msg/inbox\r\n\
BEGIN:VCARD\r\nVERSION:2.1\r\nTEL:+1\r\nEND:VCARD\r\n\
BEGIN:BENV\r\nBEGIN:VCARD\r\nVERSION:2.1\r\nTEL:+2\r\nEND:VCARD\r\n\
BEGIN:BBODY\r\nCHARSET:UTF-8\r\n\
BEGIN:MSG\r\nLine one\r\nLine two\r\nLine three\r\nEND:MSG\r\n\
END:BBODY\r\nEND:BENV\r\nEND:BMSG\r\n";
        let parsed = parse(raw.as_bytes());
        assert_eq!(parsed.body, "Line one\nLine two\nLine three");
    }

    #[test]
    fn missing_originator_vcard_yields_empty_sender() {
        let raw = "BEGIN:BMSG\r\nTYPE:SMS_GSM\r\n\
BEGIN:BENV\r\nBEGIN:VCARD\r\nVERSION:2.1\r\nTEL:+2\r\nEND:VCARD\r\n\
BEGIN:BBODY\r\nBEGIN:MSG\r\nHi\r\nEND:MSG\r\nEND:BBODY\r\nEND:BENV\r\nEND:BMSG\r\n";
        let parsed = parse(raw.as_bytes());
        assert_eq!(parsed.sender_addressing, None);
        assert_eq!(parsed.body, "Hi");
    }

    #[test]
    fn build_sms_push_round_trips_through_parser() {
        let payload = build_sms_push("+15551234567", "Hello world");
        let parsed = parse(&payload);
        assert_eq!(parsed.msg_type.as_deref(), Some("SMS_GSM"));
        assert_eq!(parsed.folder.as_deref(), Some("telecom/msg/outbox"));
        // The recipient TEL is in the INNER vCard (inside BENV), not
        // the originator — so parse() correctly leaves
        // sender_addressing empty since we built no originator.
        assert_eq!(parsed.sender_addressing, None);
        assert_eq!(parsed.body, "Hello world");
    }

    #[test]
    fn build_sms_push_neutralizes_embedded_end_msg() {
        let payload = build_sms_push("+1", "trying to break out\nEND:MSG\nbye");
        let raw = String::from_utf8_lossy(&payload);
        // The literal "END:MSG" should appear escaped (leading space)
        // so the parser sees only the legitimate one.
        let occurrences = raw.matches("\r\nEND:MSG\r\n").count();
        assert_eq!(
            occurrences, 1,
            "expected exactly one END:MSG terminator, got {} in {}",
            occurrences, raw
        );
        let parsed = parse(&payload);
        // Body keeps the offending line verbatim (with the leading
        // space) because that's what the wire saw.
        assert!(
            parsed.body.contains("END:MSG"),
            "body should preserve the escaped marker: {:?}",
            parsed.body
        );
    }

    #[test]
    fn parses_status_read_as_true() {
        let raw = "BEGIN:BMSG\r\nSTATUS:READ\r\nTYPE:SMS_GSM\r\n\
BEGIN:VCARD\r\nVERSION:2.1\r\nTEL:+1\r\nEND:VCARD\r\n\
BEGIN:BENV\r\nBEGIN:VCARD\r\nVERSION:2.1\r\nTEL:+2\r\nEND:VCARD\r\n\
BEGIN:BBODY\r\nBEGIN:MSG\r\nx\r\nEND:MSG\r\nEND:BBODY\r\nEND:BENV\r\nEND:BMSG\r\n";
        let parsed = parse(raw.as_bytes());
        assert_eq!(parsed.read, Some(true));
    }

    #[test]
    fn parse_does_not_panic_on_truncated_input() {
        let _ = parse(b"BEGIN:BMSG\r\nTYPE:SMS_GSM\r\nBEGIN:MSG\r\nhalf");
    }

    #[test]
    fn fuzz_parse_does_not_panic_on_random_bytes() {
        // Deterministic fuzz harness: 5000 random byte slices of
        // length 0..512. The parser is best-effort by design (it
        // tolerates malformed input from real phones), so it must
        // never panic regardless of what bytes it sees.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xbeef_beef_beef_beef);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 512) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse(&buf);
        }
    }

    #[test]
    fn fuzz_parse_does_not_panic_on_byte_flipped_valid_input() {
        // Structure-aware variant: start from a known-good bMessage
        // and flip one random byte per iteration. Catches off-by-one
        // length/header bugs that pure-random fuzzing rarely hits
        // because random bytes almost never form a valid framing.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let template = b"BEGIN:BMSG\r\nSTATUS:UNREAD\r\nTYPE:SMS_GSM\r\n\
FOLDER:telecom/msg/inbox\r\n\
BEGIN:VCARD\r\nVERSION:2.1\r\nN:Foo\r\nTEL:+15551234567\r\nEND:VCARD\r\n\
BEGIN:BENV\r\nBEGIN:VCARD\r\nVERSION:2.1\r\nTEL:+1\r\nEND:VCARD\r\n\
BEGIN:BBODY\r\nLENGTH:9\r\nBEGIN:MSG\r\nhello\r\nEND:MSG\r\nEND:BBODY\r\n\
END:BENV\r\nEND:BMSG\r\n";
        let mut rng = StdRng::seed_from_u64(0xfeed_face);
        for _ in 0..2_000 {
            let mut buf = template.to_vec();
            let idx = (rng.next_u32() as usize) % buf.len();
            buf[idx] ^= (rng.next_u32() & 0xff) as u8;
            let _ = parse(&buf);
        }
    }

    #[test]
    fn fuzz_parse_does_not_panic_on_truncations() {
        // Truncate a known-good bMessage at every possible cut point
        // — the parser must accept partial wire data without
        // crashing even if it can't recover the message.
        let template = b"BEGIN:BMSG\r\nSTATUS:UNREAD\r\nTYPE:SMS_GSM\r\n\
BEGIN:VCARD\r\nVERSION:2.1\r\nTEL:+1\r\nEND:VCARD\r\n\
BEGIN:BENV\r\nBEGIN:VCARD\r\nVERSION:2.1\r\nTEL:+2\r\nEND:VCARD\r\n\
BEGIN:BBODY\r\nBEGIN:MSG\r\nhi\r\nEND:MSG\r\nEND:BBODY\r\nEND:BENV\r\nEND:BMSG\r\n";
        for cut in 0..=template.len() {
            let _ = parse(&template[..cut]);
        }
    }

    #[test]
    fn build_mms_push_emits_mms_envelope_with_mime_multipart_body() {
        // Fixed timestamp keeps the boundary string and Date header
        // deterministic so we can assert the exact wire bytes.
        let now = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let bytes = build_mms_push_at("+15551234567", "Hi there", now);
        let raw = String::from_utf8(bytes).expect("output must be valid utf-8");

        // Outer envelope picks the MMS type (this is the whole
        // point — Pixel routes via its MMS dispatcher when it sees
        // TYPE:MMS, which is what unlocks the RCS-fallback thread).
        assert!(
            raw.contains("\r\nTYPE:MMS\r\n"),
            "missing TYPE:MMS in {}",
            raw
        );
        assert!(
            raw.contains("\r\nFOLDER:telecom/msg/outbox\r\n"),
            "missing outbox folder"
        );
        assert!(
            raw.contains("TEL:+15551234567\r\n"),
            "recipient TEL not in vCard"
        );

        // Inner MIME body must carry a single text/plain part with
        // the reply text, surrounded by the boundary markers. The
        // boundary is derived from the timestamp so it's stable
        // here.
        assert!(
            raw.contains("Content-Type: multipart/mixed; boundary=\"aokie-mms-1700000000\""),
            "missing top-level multipart Content-Type in {}",
            raw
        );
        assert!(
            raw.contains(
                "--aokie-mms-1700000000\r\n\
                          Content-Type: text/plain; charset=\"utf-8\"\r\n\
                          Content-Transfer-Encoding: 8BIT\r\n\
                          \r\n\
                          Hi there\r\n\
                          --aokie-mms-1700000000--\r\n"
            ),
            "MIME part body not formatted as expected: {}",
            raw
        );

        // Date header should be RFC 2822 from the injected time —
        // 2023-11-14 22:13:20 UTC.
        assert!(
            raw.contains("Date: Tue, 14 Nov 2023 22:13:20 +0000"),
            "Date header not in RFC 2822 form: {}",
            raw
        );

        // LENGTH header must be present and numeric — its exact
        // value follows the same convention as build_sms_push (the
        // length of the MIME body before the unconditional trailing
        // CRLF append) so we just sanity-check the parse. The end-
        // to-end correctness is exercised by Pixel actually
        // accepting the bMessage in the live test.
        let length_marker = "LENGTH:";
        let length_pos = raw.find(length_marker).expect("LENGTH header missing");
        let length_str: String = raw[length_pos + length_marker.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let declared_length: usize = length_str.parse().expect("LENGTH not numeric");
        assert!(
            declared_length > 0,
            "LENGTH should reflect a non-empty body"
        );
    }

    #[test]
    fn build_mms_push_neutralizes_embedded_end_msg_in_body() {
        // A reply text containing "END:MSG" should not leak the
        // outer terminator the same way build_sms_push handles it.
        let now = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let bytes = build_mms_push_at("+1", "evil\nEND:MSG\nbody", now);
        let raw = String::from_utf8_lossy(&bytes);
        let occurrences = raw.matches("\r\nEND:MSG\r\n").count();
        assert_eq!(
            occurrences, 1,
            "expected exactly one END:MSG terminator, got {} in {}",
            occurrences, raw
        );
    }
}
