//! Parser for the MAP `x-bt/MAP-msg-listing` XML body.
//!
//! Phase 4c of the SMS auto-reply plan. The MAP message-listing
//! response is a tiny XML document — a `<MAP-msg-listing>` root with
//! flat `<msg>` children, each carrying everything we care about as
//! attributes (handle, datetime, sender, type, read flag). No nested
//! children. That shape is simple enough to read with a custom 200-line
//! parser instead of dragging in a full XML crate.
//!
//! Reference: MAP 1.4.2 §5.4.5 + Annex C.4. Sample document:
//!
//! ```xml
//! <MAP-msg-listing version="1.0">
//!   <msg handle="00000001"
//!        subject="Hi"
//!        datetime="20260426T143000+1000"
//!        sender_name="John Smith"
//!        sender_addressing="+15551234567"
//!        type="SMS_GSM"
//!        size="42"
//!        read="no"
//!        sent="no" />
//! </MAP-msg-listing>
//! ```
//!
//! The parser is permissive: missing attributes become `None`, unknown
//! attributes are ignored, comments and processing-instructions skipped.
//! It does NOT validate XML — a malformed document just produces
//! whatever entries it managed to read before falling off the rails.

#![allow(dead_code)] // Phase 4c — runtime integration follows in 4b.

/// One row in the listing — only the fields the SMS auto-reply needs.
/// Other attributes (subject, size, sent, etc.) are accessible via the
/// `attributes` map if a future caller needs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    /// Hex string handle, used as the OBEX Name on a follow-up
    /// GET x-bt/message. MAP requires it to be 16 hex chars but real
    /// phones sometimes pad differently — we store whatever the AG sent.
    pub handle: String,
    /// Local timestamp from the AG. ISO-8601 basic format with optional
    /// timezone, e.g. "20260426T143000+1000". Intentionally kept as a
    /// string — parsing into chrono is the caller's choice.
    pub datetime: Option<String>,
    /// Sender's display name from the AG's address book. Empty if the
    /// number isn't in the contacts (we resolve via our own
    /// ContactStore in that case).
    pub sender_name: Option<String>,
    /// Sender's phone number. May be missing on outbox entries.
    pub sender_addressing: Option<String>,
    /// MAP message type — for SMS this is "SMS_GSM" or "SMS_CDMA".
    /// Anything else (EMAIL, MMS, IM) we ignore at the runtime layer.
    pub msg_type: Option<String>,
    /// Read flag. "yes" or "no" per spec; absence means unknown.
    pub read: Option<bool>,
}

/// Parse a MAP message-listing XML body. Returns the entries in the
/// order they appeared. Garbage in → empty result; the parser never
/// panics on malformed input.
pub fn parse_listing(input: &[u8]) -> Vec<MessageEntry> {
    let text = String::from_utf8_lossy(input);
    let mut entries = Vec::new();
    let mut cursor = 0;
    let bytes = text.as_bytes();
    while let Some(start) = find_subslice(bytes, b"<msg", cursor) {
        // Bail out if this is part of the closing element name
        // (e.g. </msg>). Tag names continue until whitespace, '>' or '/'.
        let after = start + b"<msg".len();
        if after >= bytes.len() {
            break;
        }
        let next = bytes[after];
        if !next.is_ascii_whitespace() && next != b'/' && next != b'>' {
            cursor = after;
            continue;
        }
        let end = match find_byte(bytes, b'>', after) {
            Some(idx) => idx,
            None => break,
        };
        let attr_section = &text[after..end];
        if let Some(entry) = parse_msg_attributes(attr_section) {
            entries.push(entry);
        }
        cursor = end + 1;
    }
    entries
}

fn parse_msg_attributes(section: &str) -> Option<MessageEntry> {
    let mut handle: Option<String> = None;
    let mut datetime = None;
    let mut sender_name = None;
    let mut sender_addressing = None;
    let mut msg_type = None;
    let mut read = None;

    let mut chars = section.char_indices().peekable();
    while let Some(&(i, c)) = chars.peek() {
        if c.is_ascii_whitespace() || c == '/' {
            chars.next();
            continue;
        }
        // Read attribute name up to '='.
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
        // Skip whitespace + '='.
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
            // No '=', skip.
            continue;
        }
        while let Some(&(_, ch)) = chars.peek() {
            if ch.is_ascii_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        // Read quoted value.
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
        let name = &section[name_start..name_end];
        let value = &section[value_start..value_end];
        match name {
            "handle" => handle = Some(value.to_string()),
            "datetime" => datetime = Some(value.to_string()),
            "sender_name" => sender_name = Some(value.to_string()),
            "sender_addressing" => sender_addressing = Some(value.to_string()),
            "type" => msg_type = Some(value.to_string()),
            "read" => read = Some(value.eq_ignore_ascii_case("yes")),
            _ => {}
        }
    }

    handle.map(|handle| MessageEntry {
        handle,
        datetime,
        sender_name,
        sender_addressing,
        msg_type,
        read,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() || needle.is_empty() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

fn find_byte(haystack: &[u8], needle: u8, from: usize) -> Option<usize> {
    haystack[from..]
        .iter()
        .position(|b| *b == needle)
        .map(|i| i + from)
}

// =============================================================================
// Tests.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<MAP-msg-listing version="1.0">
  <msg handle="00000001"
       subject="Hi"
       datetime="20260426T143000+1000"
       sender_name="John Smith"
       sender_addressing="+15551234567"
       type="SMS_GSM"
       size="42"
       read="no"
       sent="no" />
  <msg handle="00000002"
       datetime="20260426T144500+1000"
       sender_addressing="+15559876543"
       type="SMS_GSM"
       read="yes" />
</MAP-msg-listing>"#;

    #[test]
    fn parses_two_messages_with_correct_fields() {
        let entries = parse_listing(SAMPLE.as_bytes());
        assert_eq!(entries.len(), 2);

        let first = &entries[0];
        assert_eq!(first.handle, "00000001");
        assert_eq!(first.datetime.as_deref(), Some("20260426T143000+1000"));
        assert_eq!(first.sender_name.as_deref(), Some("John Smith"));
        assert_eq!(first.sender_addressing.as_deref(), Some("+15551234567"));
        assert_eq!(first.msg_type.as_deref(), Some("SMS_GSM"));
        assert_eq!(first.read, Some(false));

        let second = &entries[1];
        assert_eq!(second.handle, "00000002");
        assert_eq!(second.sender_name, None);
        assert_eq!(second.read, Some(true));
    }

    #[test]
    fn empty_listing_produces_empty_vec() {
        let xml = b"<?xml version=\"1.0\"?><MAP-msg-listing version=\"1.0\"/>";
        assert!(parse_listing(xml).is_empty());
    }

    #[test]
    fn missing_handle_skips_entry() {
        let xml = br#"<MAP-msg-listing>
            <msg datetime="20260426T143000+1000" type="SMS_GSM" />
            <msg handle="00000003" type="SMS_GSM" />
        </MAP-msg-listing>"#;
        let entries = parse_listing(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].handle, "00000003");
    }

    #[test]
    fn handles_single_quotes_around_attribute_values() {
        let xml = br#"<MAP-msg-listing>
            <msg handle='abcd' type='SMS_GSM' />
        </MAP-msg-listing>"#;
        let entries = parse_listing(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].handle, "abcd");
        assert_eq!(entries[0].msg_type.as_deref(), Some("SMS_GSM"));
    }

    #[test]
    fn ignores_closing_msg_tag_when_msg_uses_paired_form() {
        // Some implementations emit <msg ...></msg> instead of self-closing.
        let xml = br#"<MAP-msg-listing>
            <msg handle="0001" type="SMS_GSM"></msg>
        </MAP-msg-listing>"#;
        let entries = parse_listing(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].handle, "0001");
    }

    #[test]
    fn does_not_panic_on_truncated_xml() {
        // Cut off mid-attribute. Should produce zero entries, not panic.
        let xml = b"<MAP-msg-listing><msg handle=\"abc";
        let _ = parse_listing(xml);
    }

    #[test]
    fn skips_unknown_attributes_quietly() {
        let xml = br#"<MAP-msg-listing>
            <msg handle="0001" unknown_attr="ignored" type="SMS_GSM" extra="alsoignored" />
        </MAP-msg-listing>"#;
        let entries = parse_listing(xml);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].handle, "0001");
        assert_eq!(entries[0].msg_type.as_deref(), Some("SMS_GSM"));
    }

    #[test]
    fn does_not_misread_msg_substring_in_other_tag_names() {
        // MAP-msg-listing contains "msg" as a substring; the parser
        // must not interpret <MAP-msg-listing> as a <msg> entry.
        let xml = b"<MAP-msg-listing version=\"1.0\"></MAP-msg-listing>";
        assert!(parse_listing(xml).is_empty());
    }

    #[test]
    fn fuzz_parse_listing_does_not_panic_on_random_bytes() {
        // MAP listing XML comes from the phone over OBEX; malformed
        // XML or random bytes (including invalid UTF-8) must never
        // panic the parser.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x4d41_504c_5354_4742);
        for _ in 0..3_000 {
            let len = (rng.next_u32() % 512) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_listing(&buf);
        }
    }
}
