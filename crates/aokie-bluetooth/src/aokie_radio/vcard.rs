//! vCard 2.1 reader for the PBAP phonebook stream.
//!
//! Phase 1b of the MAP/PBAP plan. Scope is **deliberately narrow** — we
//! extract enough to greet a known caller by name and (later) match
//! contacts to inbound CallerId. Full vCard 2.1 / 3.0 / 4.0 support is
//! a research-grade rabbit hole; the spec mandates only `VERSION`, `N`,
//! and `TEL` for PBAP and that's what we read. Other properties are
//! tolerated and ignored.
//!
//! What we handle:
//!   - Multiple cards in one stream (BEGIN:VCARD ... END:VCARD).
//!   - CRLF or LF line endings.
//!   - Long-line folding (continuation lines start with space or tab).
//!   - Quoted-printable bodies (CHARSET=UTF-8 + ENCODING=QUOTED-PRINTABLE
//!     is what Android phones emit for non-ASCII names).
//!   - Multiple TEL entries per card.
//!
//! What we don't handle (and intentionally drop):
//!   - BASE64 / `;ENCODING=BASE64` (used for PHOTO, ADR fields, etc.).
//!   - Group prefixes ("item1.TEL:..."); we strip the prefix.
//!   - Property parameters beyond ENCODING and CHARSET (TYPE, PREF
//!     etc. are read but not currently exposed on `Contact`).

#![allow(dead_code)] // Phase 1b — runtime integration follows.

/// One vCard entry, narrowed to the fields we actually use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contact {
    /// Display-friendly name. Prefers FN; falls back to a join of N
    /// (Given + Family). Always trimmed; never empty unless the card
    /// has no name fields at all.
    pub display_name: String,
    /// All phone numbers from TEL fields, in the order they appeared
    /// in the card. Stripped of formatting (spaces, dashes, parens) so
    /// they can be matched against an inbound CallerId.
    pub phone_numbers: Vec<String>,
}

/// Parse a vCard 2.1 stream. Returns one Contact per BEGIN/END block.
/// Cards that fail to parse internally (malformed properties, etc.) are
/// dropped silently rather than failing the whole batch — phones are
/// quirky and one bad card shouldn't kill phonebook sync.
pub fn parse_vcards(input: &str) -> Vec<Contact> {
    let lines = unfold_lines(input);
    let mut contacts = Vec::new();
    let mut current: Option<CardBuilder> = None;

    for line in lines {
        let line = line.trim();
        if line.eq_ignore_ascii_case("BEGIN:VCARD") {
            current = Some(CardBuilder::default());
        } else if line.eq_ignore_ascii_case("END:VCARD") {
            if let Some(b) = current.take() {
                if let Some(c) = b.build() {
                    contacts.push(c);
                }
            }
        } else if let Some(ref mut b) = current {
            b.absorb_line(line);
        }
    }
    contacts
}

#[derive(Default)]
struct CardBuilder {
    name_n: Option<NField>,
    name_fn: Option<String>,
    phones: Vec<String>,
}

#[derive(Default)]
struct NField {
    family: String,
    given: String,
}

impl CardBuilder {
    fn absorb_line(&mut self, line: &str) {
        let Some(colon_idx) = line.find(':') else {
            return;
        };
        let (head, value_with_colon) = line.split_at(colon_idx);
        let value = &value_with_colon[1..];

        // head = property + optional parameters, e.g. "TEL;TYPE=CELL"
        // or "item1.N" — strip a "groupN." prefix if present.
        let head = head.split_once('.').map(|(_, rest)| rest).unwrap_or(head);
        let mut head_parts = head.split(';');
        let prop = match head_parts.next() {
            Some(p) => p.trim(),
            None => return,
        };
        let params: Vec<&str> = head_parts.collect();

        let decoded = decode_value(value, &params);

        match prop.to_ascii_uppercase().as_str() {
            "N" => {
                // N:Family;Given;Middle;Prefix;Suffix
                let mut fields = decoded.split(';');
                let family = fields.next().unwrap_or("").trim().to_string();
                let given = fields.next().unwrap_or("").trim().to_string();
                self.name_n = Some(NField { family, given });
            }
            "FN" => {
                self.name_fn = Some(decoded.trim().to_string());
            }
            "TEL" => {
                let normalised = normalise_phone(&decoded);
                if !normalised.is_empty() {
                    self.phones.push(normalised);
                }
            }
            _ => {}
        }
    }

    fn build(self) -> Option<Contact> {
        let display_name = if let Some(fn_val) = self.name_fn.filter(|s| !s.is_empty()) {
            fn_val
        } else if let Some(n) = self.name_n {
            let mut s = String::new();
            if !n.given.is_empty() {
                s.push_str(&n.given);
            }
            if !n.family.is_empty() {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(&n.family);
            }
            s
        } else {
            String::new()
        };
        if display_name.is_empty() && self.phones.is_empty() {
            return None;
        }
        Some(Contact {
            display_name,
            phone_numbers: self.phones,
        })
    }
}

/// vCard line-folding: continuation lines begin with a single space
/// or tab and append to the previous logical line. Strip the leading
/// whitespace and concatenate.
fn unfold_lines(input: &str) -> Vec<String> {
    let raw_lines: Vec<&str> = input
        .split('\n')
        .map(|s| s.trim_end_matches('\r'))
        .collect();
    let mut out: Vec<String> = Vec::with_capacity(raw_lines.len());
    for line in raw_lines {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                last.push_str(&line[1..]);
                continue;
            }
        }
        out.push(line.to_string());
    }
    out
}

/// If the property declares ENCODING=QUOTED-PRINTABLE, decode the
/// value. Handles soft line breaks (`=` at end of line — already
/// folded out by `unfold_lines`), `=XX` hex escapes, and CHARSET=UTF-8
/// (the only charset Android phones emit in practice). Unknown
/// charsets fall through with the QP-decoded bytes interpreted as
/// UTF-8 best-effort.
fn decode_value(value: &str, params: &[&str]) -> String {
    let qp = params
        .iter()
        .any(|p| p.trim().eq_ignore_ascii_case("ENCODING=QUOTED-PRINTABLE"));
    if !qp {
        return value.to_string();
    }
    decode_quoted_printable(value)
}

fn decode_quoted_printable(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'=' {
            // Soft line break (`=` at end of line). After unfold_lines
            // has stitched the line back together, a stray `=` with no
            // hex follow is treated as a literal `=`.
            if i + 2 < bytes.len() {
                let h1 = hex_digit(bytes[i + 1]);
                let h2 = hex_digit(bytes[i + 2]);
                if let (Some(h1), Some(h2)) = (h1, h2) {
                    out.push((h1 << 4) | h2);
                    i += 3;
                    continue;
                }
            }
            out.push(b'=');
            i += 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    // Quoted-printable bytes are interpreted as the declared charset.
    // We assume UTF-8 (matches Android Bluedroid). Use lossy on the
    // off-chance a phone sends an unannounced legacy charset so we
    // don't poison the whole batch.
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Strip formatting characters from a phone number. Keeps the leading
/// `+` (E.164) and digits; everything else is dropped. We don't try
/// to validate the result — phones are messy ("ext.", "x123", etc.) and
/// matching CallerId is best done after the same normalisation.
fn normalise_phone(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for (i, c) in input.chars().enumerate() {
        if c == '+' && i == 0 {
            out.push('+');
        } else if c.is_ascii_digit() {
            out.push(c);
        }
    }
    out
}

// =============================================================================
// Tests.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_vcard_with_n_and_tel() {
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
N:Doe;Jane\r\n\
TEL:+15551234567\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(
            cards,
            vec![Contact {
                display_name: "Jane Doe".into(),
                phone_numbers: vec!["+15551234567".into()],
            }]
        );
    }

    #[test]
    fn fn_takes_priority_over_n_for_display_name() {
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
N:Doe;Jane;Marie\r\n\
FN:Jane Doe (Mobile)\r\n\
TEL:+15551234567\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards[0].display_name, "Jane Doe (Mobile)");
    }

    #[test]
    fn parses_multiple_vcards_in_one_stream() {
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
FN:Alice\r\n\
TEL:+1\r\n\
END:VCARD\r\n\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
FN:Bob\r\n\
TEL:+2\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].display_name, "Alice");
        assert_eq!(cards[1].display_name, "Bob");
    }

    #[test]
    fn collects_multiple_phones_per_card() {
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
FN:Three Phones\r\n\
TEL;TYPE=HOME:+15550000001\r\n\
TEL;TYPE=WORK:(555) 000-0002\r\n\
TEL;TYPE=CELL:+1-555-000-0003\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(
            cards[0].phone_numbers,
            vec![
                "+15550000001".to_string(),
                "5550000002".to_string(),
                "+15550000003".to_string(),
            ]
        );
    }

    #[test]
    fn unfolds_continuation_lines() {
        // RFC 2426 line folding: continuation lines start with space.
        // The vCard 2.1 parser must rejoin them before reading the
        // property — pre-fold it'd see "FN:Jane" + " Doe" as two
        // separate properties and the second would be ignored.
        let stream = "BEGIN:VCARD\r\nVERSION:2.1\r\nFN:Jane\r\n Doe\r\nTEL:+1\r\nEND:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards[0].display_name, "JaneDoe");
    }

    #[test]
    fn decodes_quoted_printable_utf8_for_non_ascii_names() {
        // Real Pixel 10a output for a contact "Jürgen": the AG sends
        // CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE and the value is the
        // QP-encoded UTF-8 bytes of the string.
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
N;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:M=C3=BCller;J=C3=BCrgen\r\n\
FN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:J=C3=BCrgen M=C3=BCller\r\n\
TEL:+15551234567\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards[0].display_name, "Jürgen Müller");
    }

    #[test]
    fn handles_lf_only_line_endings() {
        // BTstack and some older Bluedroid builds emit LF-only.
        let stream = "BEGIN:VCARD\nVERSION:2.1\nFN:LF Only\nTEL:+1\nEND:VCARD\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].display_name, "LF Only");
    }

    #[test]
    fn strips_group_prefixes() {
        // Apple vCards use "item1.TEL:..." style group syntax.
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
item1.FN:Group Name\r\n\
item2.TEL:+15551234567\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards[0].display_name, "Group Name");
        assert_eq!(cards[0].phone_numbers, vec!["+15551234567".to_string()]);
    }

    #[test]
    fn ignores_unknown_properties_without_failing_card() {
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
PHOTO;ENCODING=BASE64:bnVrZS1tZS1mcm9tLW9yYml0\r\n\
ADR:;;123 Main St;City;ST;00000;US\r\n\
FN:Has Photo\r\n\
TEL:+15550000000\r\n\
NOTE:multi line note doesn't break us\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].display_name, "Has Photo");
        assert_eq!(cards[0].phone_numbers, vec!["+15550000000".to_string()]);
    }

    #[test]
    fn drops_card_with_no_name_or_phone() {
        // A truly empty card is useless; don't pollute the contact list.
        let stream = "BEGIN:VCARD\r\nVERSION:2.1\r\nEND:VCARD\r\n";
        assert!(parse_vcards(stream).is_empty());
    }

    #[test]
    fn falls_back_to_n_when_fn_absent() {
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
N:Surname;Forename\r\n\
TEL:+1\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards[0].display_name, "Forename Surname");
    }

    #[test]
    fn n_with_only_family_or_given_still_yields_a_name() {
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
N:Surname;\r\n\
TEL:+1\r\n\
END:VCARD\r\n\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
N:;Forename\r\n\
TEL:+2\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards[0].display_name, "Surname");
        assert_eq!(cards[1].display_name, "Forename");
    }

    #[test]
    fn malformed_property_lines_are_skipped_not_fatal() {
        // No colon → no property; just keep going.
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
this is garbage\r\n\
FN:Survives\r\n\
TEL:+1\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        assert_eq!(cards[0].display_name, "Survives");
    }

    #[test]
    fn quoted_printable_with_invalid_hex_escapes_keeps_literal_equals() {
        // Bare `=` followed by non-hex should not panic or eat the next
        // chars. Real phones sometimes botch QP at line boundaries.
        let stream = "\
BEGIN:VCARD\r\n\
VERSION:2.1\r\n\
FN;ENCODING=QUOTED-PRINTABLE:Foo=ZZ=42Bar\r\n\
TEL:+1\r\n\
END:VCARD\r\n";
        let cards = parse_vcards(stream);
        // =ZZ is not valid hex → preserved literally; =42 = 'B' →
        // decodes; result is "Foo=ZZBBar".
        assert_eq!(cards[0].display_name, "Foo=ZZBBar");
    }

    #[test]
    fn fuzz_parse_vcards_does_not_panic_on_random_strings() {
        // vCard parsing handles QP/Base64/UTF-16 — plenty of decode
        // paths. Random ASCII strings must never crash.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x5643_4152_4400_5643);
        for _ in 0..3_000 {
            let len = (rng.next_u32() % 1024) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            // Parser is &str so we keep bytes printable-ish to feed
            // it valid UTF-8; the goal is to fuzz the structure, not
            // the UTF-8 decoder.
            let s: String = buf
                .iter()
                .map(|b| char::from(b.saturating_add(0x20).min(0x7e)))
                .collect();
            let _ = parse_vcards(&s);
        }
    }

    #[test]
    fn fuzz_parse_vcards_does_not_panic_on_byte_flipped_valid_input() {
        // Mutate one byte in a known-good vCard stream — exercises
        // the actual structured parsing paths that random ASCII
        // mostly skips.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let template = "BEGIN:VCARD\r\nVERSION:2.1\r\nN:Smith;John\r\n\
FN:John Smith\r\nTEL;TYPE=CELL:+15551234567\r\n\
EMAIL:john@example.com\r\nEND:VCARD\r\n";
        let mut rng = StdRng::seed_from_u64(0x5642_4954_5642_4954);
        for _ in 0..2_000 {
            let mut buf = template.as_bytes().to_vec();
            let idx = (rng.next_u32() as usize) % buf.len();
            buf[idx] ^= (rng.next_u32() & 0x7f) as u8;
            // Keep ASCII; vcard parser takes &str so non-UTF-8 would
            // never reach it via from_utf8_lossy at the call site.
            if let Ok(s) = std::str::from_utf8(&buf) {
                let _ = parse_vcards(s);
            }
        }
    }
}
