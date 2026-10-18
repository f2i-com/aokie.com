//! One rule for display text that crosses the Companion boundary.
//!
//! Every limit on the wire is in UTF-8 **bytes** (the JSON schemas, the
//! protocol validators and the gateway all count bytes), so a sender that cut
//! at a character count could still overflow by up to four times on CJK or
//! emoji text. Display text is also one line: the phone shows captions,
//! reasons and summaries in single-line rows, and older phones refuse any
//! control character outright. Senders run [`sanitize_line`] before they
//! publish; receivers that must stay lenient with older senders run it too
//! instead of refusing the whole frame.

/// True for the characters display text never carries: every Unicode control
/// character (C0 including tab and newline, DEL, C1) and the two Unicode
/// line/paragraph separators, which render as line breaks.
pub fn is_forbidden_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

/// The longest prefix of `value` that fits in `max_bytes` without splitting a
/// character.
pub fn truncate_to_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Make `value` safe to publish as one line of display text within
/// `max_bytes`: each run of forbidden characters becomes one space, the ends
/// are trimmed, and the result is cut on a character boundary. The result may
/// be empty; callers that need text drop it then.
pub fn sanitize_line(value: &str, max_bytes: usize) -> String {
    let mut line = String::with_capacity(value.len().min(max_bytes));
    let mut pending_space = false;
    for c in value.chars() {
        if is_forbidden_char(c) {
            pending_space = true;
            continue;
        }
        if line.is_empty() && c.is_whitespace() {
            // Leading blanks are trimmed as they arrive, so the early stop
            // below always leaves a full line of text to cut from.
            continue;
        }
        if pending_space && !line.is_empty() {
            line.push(' ');
        }
        pending_space = false;
        line.push(c);
        if line.len() > max_bytes.saturating_add(4) {
            // Enough to cut from; the rest can never be published.
            break;
        }
    }
    truncate_to_bytes(line.trim_end(), max_bytes)
        .trim_end()
        .to_owned()
}

/// Why [`check_line`] refused a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextBoundsError {
    Empty,
    TooLong,
    ForbiddenCharacter,
}

/// The strict twin of [`sanitize_line`]: `value` must be non-empty, at most
/// `max_bytes` UTF-8 bytes, and free of forbidden characters. Anything
/// [`sanitize_line`] returns non-empty passes.
pub fn check_line(value: &str, max_bytes: usize) -> Result<(), TextBoundsError> {
    if value.is_empty() {
        Err(TextBoundsError::Empty)
    } else if value.len() > max_bytes {
        Err(TextBoundsError::TooLong)
    } else if value.chars().any(is_forbidden_char) {
        Err(TextBoundsError::ForbiddenCharacter)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_counts_bytes_and_never_splits_a_character() {
        assert_eq!(truncate_to_bytes("abc", 10), "abc");
        assert_eq!(truncate_to_bytes("abc", 2), "ab");
        // "日" is three bytes; four bytes hold one of them, not one and a third.
        assert_eq!(truncate_to_bytes("日本", 4), "日");
        assert_eq!(truncate_to_bytes("日本", 2), "");
        // A four-byte emoji is kept whole or dropped whole.
        assert_eq!(truncate_to_bytes("a😀", 4), "a");
        assert_eq!(truncate_to_bytes("a😀", 5), "a😀");
    }

    #[test]
    fn sanitizing_folds_line_breaks_and_controls_into_single_spaces() {
        assert_eq!(sanitize_line("one\ntwo", 100), "one two");
        assert_eq!(sanitize_line("one\r\n\ttwo", 100), "one two");
        assert_eq!(sanitize_line("a\u{2028}b\u{2029}c", 100), "a b c");
        assert_eq!(sanitize_line("x\u{0}y\u{7f}z\u{85}w", 100), "x y z w");
        assert_eq!(sanitize_line("\n\n lead and trail \n", 100), "lead and trail");
        assert_eq!(sanitize_line("\n\t\r", 100), "");
        // Ordinary punctuation, curly quotes and accents are untouched.
        assert_eq!(sanitize_line("“Café” — ok", 100), "“Café” — ok");
    }

    #[test]
    fn a_long_cjk_turn_fits_the_byte_limit_and_passes_the_strict_check() {
        // 2,000 characters of CJK is 6,000 bytes: the old character cut let it
        // through and the receiver's byte check then refused the snapshot.
        let turn: String = "語".repeat(2_000);
        let line = sanitize_line(&turn, 2_000);
        assert!(line.len() <= 2_000);
        assert_eq!(line.chars().count(), 666);
        assert_eq!(check_line(&line, 2_000), Ok(()));

        let multi = format!("{}\n{}", "“quoted”".repeat(200), "é".repeat(800));
        let line = sanitize_line(&multi, 2_000);
        assert!(line.len() <= 2_000);
        assert_eq!(check_line(&line, 2_000), Ok(()));
    }

    #[test]
    fn the_strict_check_names_each_failure() {
        assert_eq!(check_line("", 10), Err(TextBoundsError::Empty));
        assert_eq!(check_line("12345678901", 10), Err(TextBoundsError::TooLong));
        assert_eq!(check_line("日本語の", 10), Err(TextBoundsError::TooLong));
        assert_eq!(
            check_line("two\nlines", 100),
            Err(TextBoundsError::ForbiddenCharacter)
        );
        assert_eq!(check_line("tab\there", 100), Err(TextBoundsError::ForbiddenCharacter));
        assert_eq!(check_line("fine", 10), Ok(()));
    }

    #[test]
    fn a_huge_input_is_cut_without_copying_all_of_it() {
        let huge = "a".repeat(1_000_000);
        let line = sanitize_line(&huge, 2_000);
        assert_eq!(line.len(), 2_000);
        let padded = format!("{}\n\n{huge}", " ".repeat(50));
        assert_eq!(sanitize_line(&padded, 2_000).len(), 2_000);
    }
}
