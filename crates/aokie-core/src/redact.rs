//! Lightweight PII redaction helpers for logs.
//!
//! The default app build ships with `eprintln!`/`println!` going
//! straight to stderr/stdout. Several call sites quote raw phone
//! numbers, SMS bodies, and Whisper transcripts in full — fine for a
//! lab build but not what we want in a production deployment whose
//! logs might be captured by a monitoring tool, copy-pasted into a
//! support thread, or shipped off in a crash dump.
//!
//! These helpers do two things:
//!
//! 1. Provide format wrappers (`Phone(&str)`, `Text(&str)`) that
//!    redact at format-time. Cheap to drop into existing `println!`
//!    sites — no log-level plumbing required.
//!
//! 2. Honor an `AOKIE_VERBOSE_LOGS=1` env var that bypasses redaction
//!    entirely. Local dev / hardware debugging stays full-fidelity;
//!    production logs stay PII-free.
//!
//! Redaction is deliberately conservative: phone numbers keep the
//! last 4 digits (operator can still tell calls apart in the log
//! stream), SMS bodies / transcripts keep a length count.

use std::fmt;
use std::sync::OnceLock;

/// Cached `AOKIE_VERBOSE_LOGS` lookup. Reading the env every log call
/// is cheap on Windows but pointless — once at startup is enough.
fn verbose_enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| match std::env::var("AOKIE_VERBOSE_LOGS") {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    })
}

/// Print a one-time banner if `AOKIE_VERBOSE_LOGS` is set. Called from
/// `lib.rs::run` at startup so operators see "PII is leaking into the
/// log stream" before any sensitive call data lands. The banner is
/// loud on purpose — prod builds should never run with this flag set,
/// and a quiet `eprintln` would be easy to miss in a busy console.
pub fn announce_log_mode() {
    if !verbose_enabled() {
        return;
    }
    eprintln!("══════════════════════════════════════════════════════════════════");
    eprintln!("  AOKIE_VERBOSE_LOGS=1 — REDACTION DISABLED");
    eprintln!("  Phone numbers, SMS bodies, and transcripts will be logged in");
    eprintln!("  full to stdout/stderr. Intended for local hardware debugging");
    eprintln!("  only. Unset the env var before shipping logs anywhere.");
    if !cfg!(debug_assertions) {
        // Release-build operators are far more likely to be tricked by
        // a support tech into running with this on. Make the warning
        // even louder when the build is one a customer might be
        // running, and remind them never to upload the resulting log.
        eprintln!("  RELEASE BUILD: this is a packaged install. Do not ship the");
        eprintln!("  resulting log to anyone — including Aokie support — without");
        eprintln!("  reviewing every line. The redacted log is the safe one.");
    }
    eprintln!("══════════════════════════════════════════════════════════════════");
}

/// Public accessor used by the Tauri command surface so the renderer
/// can paint a top-of-app banner whenever PII redaction is bypassed.
/// `verbose_enabled` is private (it's the cached env-var lookup); this
/// is the deliberate IPC-friendly entry point.
pub fn verbose_logs_active() -> bool {
    verbose_enabled()
}

/// True when `AOKIE_DUMP_RX_WAV=1` is set AND the build will honour
/// it — i.e. caller audio is actively being written to disk. Release
/// builds always return false (the dump is gated behind
/// `cfg(debug_assertions)`), so the renderer can use the result as a
/// "is this build leaking caller PCM right now?" signal regardless
/// of whether the operator actually compiled debug or release. The
/// banner this drives is non-dismissible (R3-#16): an operator
/// running a debug build with the dump on must keep seeing the
/// warning so a long-running session can't quietly accumulate
/// caller audio for hours.
pub fn rx_wav_dump_active() -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("AOKIE_DUMP_RX_WAV")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// Print scary banners for the other debug knobs that can leak PII or
/// audio:
///
///   * `AOKIE_RADIO_DUMP=1` hex-dumps every HCI/ACL/SCO transfer to
///     stderr. The hex dump itself is not PII but the volume is huge
///     (every Bluetooth byte) and it can capture the framing of message
///     bodies and transcripts in flight.
///   * `AOKIE_DUMP_RX_WAV=1` writes raw + 16 kHz Whisper-input WAV
///     files for every speech segment under `%TEMP%/aokie_rx_dumps`.
///     That's caller audio on disk in the clear — definitely not for
///     a real deployment. We also sweep the directory clean on every
///     launch so old dumps don't accumulate quietly between debug
///     sessions.
pub fn announce_debug_dumps() {
    let radio_dump = std::env::var_os("AOKIE_RADIO_DUMP").is_some();
    let rx_wav_dump = std::env::var("AOKIE_DUMP_RX_WAV")
        .map(|v| v == "1")
        .unwrap_or(false);

    if radio_dump {
        if !cfg!(debug_assertions) {
            // Release builds refuse to honour the env var entirely so a
            // packaged install can never be coerced into hex-dumping
            // HCI/ACL/SCO transfers — the dump captures message-body
            // framing in flight. Surface the ignored attempt loudly so
            // an operator who set it expecting it to work doesn't
            // wonder why no dumps appear.
            eprintln!("══════════════════════════════════════════════════════════════════");
            eprintln!("  AOKIE_RADIO_DUMP is IGNORED in release builds.");
            eprintln!("  Run a debug build if you need HCI/ACL/SCO hex dumps for");
            eprintln!("  hardware debugging. The dump captures the framing of message");
            eprintln!("  bodies in flight — too sensitive for a packaged install.");
            eprintln!("══════════════════════════════════════════════════════════════════");
        } else {
            eprintln!("══════════════════════════════════════════════════════════════════");
            eprintln!("  AOKIE_RADIO_DUMP=1 — every HCI/ACL/SCO transfer is being hex-");
            eprintln!("  dumped to stderr. Volume is high and the dump captures the");
            eprintln!("  framing of message bodies in flight. Debug builds only —");
            eprintln!("  unset before shipping logs anywhere, even from a dev box.");
            eprintln!("══════════════════════════════════════════════════════════════════");
        }
    }

    if rx_wav_dump {
        if !cfg!(debug_assertions) {
            // Release builds refuse to honour the env var entirely,
            // so a packaged install can never be coerced into writing
            // caller audio to disk. Surface the ignored attempt loudly
            // so an operator who set it expecting it to work doesn't
            // wonder why no dumps appear.
            eprintln!("══════════════════════════════════════════════════════════════════");
            eprintln!("  AOKIE_DUMP_RX_WAV is IGNORED in release builds.");
            eprintln!("  Run a debug build if you need caller-audio dumps for VAD /");
            eprintln!("  Whisper debugging. Caller PII on disk is too high-risk to ship.");
            eprintln!("══════════════════════════════════════════════════════════════════");
            return;
        }
        let dir = std::env::temp_dir().join("aokie_rx_dumps");
        // Sweep stale dumps from prior runs — they're caller audio on
        // disk, and a debug session left enabled overnight would
        // otherwise pile up indefinitely. Best-effort: a missing
        // directory or a permission glitch is fine, the next dump call
        // will just recreate it.
        let cleared = match std::fs::read_dir(&dir) {
            Ok(entries) => {
                let mut n = 0usize;
                for entry in entries.flatten() {
                    if std::fs::remove_file(entry.path()).is_ok() {
                        n += 1;
                    }
                }
                n
            }
            Err(_) => 0,
        };
        eprintln!("══════════════════════════════════════════════════════════════════");
        eprintln!("  AOKIE_DUMP_RX_WAV=1 — caller audio is being written to disk at");
        eprintln!("  {}", dir.display());
        if cleared > 0 {
            eprintln!("  Swept {} stale WAV file(s) from prior runs.", cleared);
        }
        eprintln!("  Debug builds only. Treat the dump directory as PII and delete");
        eprintln!("  it when finished.");
        eprintln!("══════════════════════════════════════════════════════════════════");
    }
}

/// Format wrapper: emits the phone with all but the last 4 digits
/// masked. `+61432***1234` for `+614321231234`. Non-digit chars
/// (leading `+`, spaces, parentheses) are kept on the leading side
/// so the masked form still looks like a phone number.
pub struct Phone<'a>(pub &'a str);

impl fmt::Display for Phone<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if verbose_enabled() {
            return f.write_str(self.0);
        }
        let s = self.0;
        let digit_count = s.chars().filter(|c| c.is_ascii_digit()).count();
        if digit_count <= 4 {
            // Too short to redact meaningfully — just show length.
            return write!(f, "<phone {} digits>", digit_count);
        }
        // Walk left-to-right, keeping every leading prefix char and
        // masking digits until we're within the last 4 digits.
        let mut digits_seen = 0usize;
        let target_visible_from = digit_count - 4;
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if c.is_ascii_digit() {
                if digits_seen < target_visible_from {
                    out.push('*');
                } else {
                    out.push(c);
                }
                digits_seen += 1;
            } else {
                out.push(c);
            }
        }
        f.write_str(&out)
    }
}

/// Format wrapper: emits a length-only summary of a free-text payload
/// (SMS body, transcript line, model output snippet) when verbose
/// logs are off; emits the full text when on.
pub struct Text<'a>(pub &'a str);

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if verbose_enabled() {
            return f.write_str(self.0);
        }
        let chars = self.0.chars().count();
        write!(f, "<{} chars>", chars)
    }
}

/// Validate a phone number supplied to the SMS dispatch path. Trims
/// surrounding whitespace, rejects empty / oversized inputs, and
/// allows only the conservative set of characters real phone numbers
/// use. Returns the trimmed string for the caller to use directly.
///
/// We intentionally do not normalise to E.164 here — that's
/// `database::normalize_number`'s job. This validator is the
/// boundary check: anything that wouldn't survive being passed into
/// AT command arguments or the MAP bMessage envelope gets rejected
/// upfront with a clear error rather than reaching a deeper layer.
// SMS validators are only called from the Windows BT send-SMS path
// (`bluetooth_send_sms`); on Linux they're cargo-dead-code despite
// having tests below. Suppress on Linux without dropping the surface.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn validate_sms_recipient(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("recipient_phone is empty".to_string());
    }
    // 32 chars is generous — E.164 caps at 15 digits, so 32 leaves
    // room for `+`, dashes, spaces, and parens.
    if trimmed.len() > 32 {
        return Err(format!(
            "recipient_phone is too long ({} chars; max 32)",
            trimmed.len()
        ));
    }
    let ok = trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | ' ' | '(' | ')' | '.'));
    if !ok {
        return Err(
            "recipient_phone contains disallowed characters (only digits, +, -, space, parens, dot are accepted)"
                .to_string(),
        );
    }
    let digit_count = trimmed.chars().filter(|c| c.is_ascii_digit()).count();
    if digit_count < 4 {
        return Err(format!(
            "recipient_phone has only {} digits (minimum 4)",
            digit_count
        ));
    }
    Ok(trimmed.to_string())
}

/// Cap on the SMS body length we'll accept from the renderer. Real
/// SMS segments are 160 GSM-7 chars, MMS is much larger but capped
/// per carrier; we accept up to ~6 KB which covers any realistic
/// bMessage push without letting the renderer queue megabyte-sized
/// payloads against the AG.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub const SMS_BODY_MAX_BYTES: usize = 6 * 1024;

/// Validate an SMS body. Empty bodies are rejected (the AG will
/// silently drop them and the operator wonders why "send" did
/// nothing). Oversized bodies are rejected at the boundary so a
/// runaway transcript can't land in the runtime SMS queue.
///
/// R5-#9: also rejects embedded C0 control characters (NUL, BEL, DEL,
/// SOH..) other than `\n` / `\r` / `\t`. The bMessage encoder + the
/// AT-CMGS wire path both treat raw bytes as text, so a NUL embedded
/// in the body would either truncate the message at the modem
/// boundary or smuggle a stray control byte across the line. The
/// allowlist mirrors the recipient validator's "if it looks like
/// terminal injection, refuse the message" stance.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn validate_sms_body(input: &str) -> Result<String, String> {
    let trimmed = input.trim_end_matches(['\r', '\n', ' ']);
    if trimmed.trim().is_empty() {
        return Err("body is empty".to_string());
    }
    if trimmed.len() > SMS_BODY_MAX_BYTES {
        return Err(format!(
            "body is too long ({} bytes; max {})",
            trimmed.len(),
            SMS_BODY_MAX_BYTES
        ));
    }
    if let Some((idx, ch)) = trimmed
        .char_indices()
        .find(|(_, c)| c.is_control() && *c != '\n' && *c != '\r' && *c != '\t')
    {
        return Err(format!(
            "body contains control character {:?} at byte {}",
            ch, idx
        ));
    }
    Ok(trimmed.to_string())
}

/// Audit-log a sensitive operation. Prints a single line tagged
/// `[Audit]` to stderr so it can be filtered out of debug logs into
/// a separate stream by an operator.
///
/// **The caller is responsible for redaction.** This function does
/// not wrap `detail` in `Phone(...)` / `Text(...)` automatically —
/// passing raw PII straight in will log it raw regardless of the
/// `AOKIE_VERBOSE_LOGS` setting. If the detail might carry a phone
/// number or message body, format it through the redaction wrappers
/// at the call site:
///
/// ```ignore
/// audit("sms_send", format!("to={} bytes={}", Phone(&recipient), body.len()));
/// ```
///
/// Existing call sites only log structural metadata (paths, byte
/// sizes, version numbers, action names), which is fine — but new
/// call sites should look at what they're passing before calling.
///
/// Future: emit a Tauri event the UI can render in an audit panel.
/// For now stderr is the floor that lets ops run `findstr [Audit]`
/// without needing UI work first.
pub fn audit(operation: &str, detail: impl AsRef<str>) {
    eprintln!("[Audit] {} :: {}", operation, detail.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(phone: &str) -> String {
        format!("{}", Phone(phone))
    }

    #[test]
    fn redacts_aussie_mobile() {
        // Default (verbose off) — keeps the last 4 digits, masks the rest.
        // We can't toggle verbose here because the cache is OnceLock, so
        // these tests rely on the default-off state.
        let s = render("+61432123456");
        assert!(s.ends_with("3456"), "got: {}", s);
        assert!(s.contains('*'));
        assert!(s.starts_with('+'));
    }

    #[test]
    fn keeps_short_phone_as_length_only() {
        let s = render("123");
        assert!(s.contains("3 digits"), "got: {}", s);
    }

    #[test]
    fn text_redaction_emits_length() {
        let s = format!("{}", Text("hello world"));
        assert!(s.contains("11 chars"), "got: {}", s);
    }

    #[test]
    fn sms_recipient_rejects_empty() {
        assert!(validate_sms_recipient("").is_err());
        assert!(validate_sms_recipient("   ").is_err());
    }

    #[test]
    fn sms_recipient_rejects_garbage() {
        assert!(validate_sms_recipient("DROP TABLE sms").is_err());
        assert!(validate_sms_recipient("../../etc/passwd").is_err());
        assert!(validate_sms_recipient("\n\rINJECT\n").is_err());
    }

    #[test]
    fn sms_recipient_accepts_e164() {
        assert_eq!(
            validate_sms_recipient(" +61432123456 ").unwrap(),
            "+61432123456"
        );
        assert_eq!(
            validate_sms_recipient("(02) 8123-4567").unwrap(),
            "(02) 8123-4567"
        );
    }

    #[test]
    fn sms_recipient_rejects_too_short() {
        assert!(validate_sms_recipient("123").is_err());
    }

    #[test]
    fn sms_body_rejects_empty() {
        assert!(validate_sms_body("").is_err());
        assert!(validate_sms_body("   \n").is_err());
    }

    #[test]
    fn sms_body_caps_oversized() {
        let huge = "a".repeat(SMS_BODY_MAX_BYTES + 1);
        assert!(validate_sms_body(&huge).is_err());
        let just_fits = "b".repeat(SMS_BODY_MAX_BYTES);
        assert!(validate_sms_body(&just_fits).is_ok());
    }

    /// R5-#9: NUL, BEL, DEL, etc. must not slip through. \n / \r /
    /// \t are legitimate (multiline SMS bodies happen) so they stay.
    #[test]
    fn sms_body_rejects_control_chars() {
        assert!(validate_sms_body("hello\x00world").is_err());
        assert!(validate_sms_body("alert\x07").is_err());
        assert!(validate_sms_body("ack\x06").is_err());
        // Allowed whitespace must still pass.
        assert!(validate_sms_body("first line\nsecond line").is_ok());
        assert!(validate_sms_body("col1\tcol2").is_ok());
        assert!(validate_sms_body("crlf\r\nnext").is_ok());
    }
}
