//! Classify a base URL by where it actually points: loopback /
//! private (RFC 1918 / RFC 4193) / link-local / cloud-instance metadata
//! / public / invalid.
//!
//! R13/7: this module exists because the renderer is treated as
//! untrusted in the threat model. The frontend already classifies
//! `ai_providers.json` URLs in `src/lib/urlClassification.ts` and
//! refuses to save metadata endpoints, but a tampered renderer (or a
//! `cargo tauri-plugin` shim that bypasses the React save flow) could
//! still call `ai_set_providers_override` with an SSRF-shaped URL.
//! Mirroring the classifier in Rust lets the command boundary refuse
//! the same payloads without trusting the renderer.
//!
//! We intentionally do NOT collapse this with the existing
//! `is_loopback_or_private_host` helper in `commands::bluetooth_commands`
//! — that helper returns a coarser bool used by the readiness gate
//! (loopback/private = "auth optional"), and reconciling the two
//! call sites is a separate refactor. The two implementations agree
//! on the security-critical buckets (no fake-prefix DNS spoof, real
//! IP literal parsing only); shared regression tests live below.
//!
//! Mirrors `src/lib/urlClassification.ts` — keep them in sync.

use std::net::{Ipv4Addr, Ipv6Addr};

/// Output of [`classify_base_url`]. The variants are deliberately the
/// same shape as the TypeScript `BaseUrlClassification` union so a
/// future refactor that hoists this into a shared schema is
/// mechanical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseUrlClassification {
    /// Empty / whitespace-only — nothing to evaluate yet.
    Empty,
    /// 127.0.0.0/8, ::1, exact `localhost`, 0.0.0.0. On-device.
    Loopback,
    /// Cloud-instance metadata endpoints (AWS / GCP `metadata.google.internal`
    /// / 169.254.169.254). High-risk: pointing the LLM here forwards
    /// caller transcripts to the metadata API and, on a credentialed
    /// VM, exfiltrates IAM tokens via SSRF.
    Metadata,
    /// 169.254.x.x other than 169.254.169.254, plus IPv6 fe80::/10.
    /// Almost always a misconfiguration.
    LinkLocal,
    /// RFC 1918 (10.x, 192.168.x, 172.16-31.x) / RFC 4193 (fc00::/7).
    /// Probably a homelab / on-LAN endpoint; the Settings UI warns
    /// that data leaves the host.
    Private,
    /// Anything else — true cloud / WAN, or a hostname an attacker
    /// could be controlling.
    Public,
    /// URL doesn't parse and looks neither loopback nor anything we
    /// recognise. Treated as "block" by callers that want to be
    /// strict.
    Invalid,
}

/// Classify a base URL string. The input is whatever the operator
/// typed (or the renderer round-tripped); we don't require a scheme.
///
/// Security note: hostnames (anything that isn't a literal IP or the
/// exact strings `localhost` / `metadata.google.internal`) classify
/// as [`BaseUrlClassification::Public`] regardless of how they spell.
/// This is the bug fix from R10/High-1 — `127.0.0.1.evil.com`,
/// `10.example.com`, and `fcfoo.com` are valid attacker-controlled
/// DNS names whose prefix matches a naive substring check, and the
/// previous prefix classifier returned the trusted bucket for them.
/// A real `Ipv4Addr` / `Ipv6Addr` parse is the only way to be sure
/// the host is what it looks like.
pub fn classify_base_url(raw: &str) -> BaseUrlClassification {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return BaseUrlClassification::Empty;
    }

    // Hand-parse the host portion: skip past `://` if present, then
    // take everything up to the first `/`, `?`, or `#`. We don't pull
    // the `url` crate as a direct dep — it's a transitive of Tauri
    // but coupling our gate to a transitive version is fragile.
    let after_scheme = match trimmed.find("://") {
        Some(idx) => &trimmed[idx + 3..],
        None => trimmed,
    };
    let host_end = after_scheme
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(after_scheme.len());
    let host_with_port = &after_scheme[..host_end];

    if host_with_port.is_empty() {
        return BaseUrlClassification::Invalid;
    }

    // Strip optional `:<port>` while preserving an IPv6 bracket pair
    // (`[::1]:8080` → `::1`).
    let host = if host_with_port.starts_with('[') {
        // `[ipv6]:port`. Slice between the brackets — anything after
        // the `]` is port noise.
        match host_with_port.find(']') {
            Some(close) => &host_with_port[1..close],
            None => return BaseUrlClassification::Invalid,
        }
    } else if host_with_port.contains(':') && !host_with_port.starts_with(':') {
        // Could be `host:port` or a bracketless IPv6 literal. Bracketless
        // IPv6 inside a URL is malformed (RFC 3986 requires the brackets);
        // treat any colon-bearing unbracketed string as `host:port` and
        // strip after the LAST colon. This is what URL.hostname does.
        host_with_port
            .rsplit_once(':')
            .map(|(h, _port)| h)
            .unwrap_or(host_with_port)
    } else {
        host_with_port
    };

    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return BaseUrlClassification::Invalid;
    }

    classify_host(&host)
}

fn classify_host(host: &str) -> BaseUrlClassification {
    // Exact-match special hostnames — never combined with prefix checks.
    if host == "localhost" {
        return BaseUrlClassification::Loopback;
    }
    if host == "metadata.google.internal" {
        return BaseUrlClassification::Metadata;
    }

    // IPv4 literal.
    if let Some([a, b, c, d]) = parse_ipv4(host) {
        if a == 127 {
            return BaseUrlClassification::Loopback;
        }
        if a == 0 && b == 0 && c == 0 && d == 0 {
            return BaseUrlClassification::Loopback;
        }
        if a == 169 && b == 254 {
            if c == 169 && d == 254 {
                return BaseUrlClassification::Metadata;
            }
            return BaseUrlClassification::LinkLocal;
        }
        if a == 10 {
            return BaseUrlClassification::Private;
        }
        if a == 192 && b == 168 {
            return BaseUrlClassification::Private;
        }
        if a == 172 && (16..=31).contains(&b) {
            return BaseUrlClassification::Private;
        }
        return BaseUrlClassification::Public;
    }

    // IPv6 literal.
    if let Ok(addr) = host.parse::<Ipv6Addr>() {
        if addr.is_loopback() {
            return BaseUrlClassification::Loopback;
        }
        let segs = addr.segments();
        // fe80::/10 — top 10 bits = 1111111010xx. The high byte of
        // segment 0 is 0xfe; the next 2 bits live in the next nibble:
        // top 2 bits of (segs[0] & 0x00ff) >> 6 must be 0b10.
        let first_byte = (segs[0] >> 8) as u8;
        let second_byte = (segs[0] & 0x00ff) as u8;
        if first_byte == 0xfe && (second_byte & 0xc0) == 0x80 {
            return BaseUrlClassification::LinkLocal;
        }
        // fc00::/7 — top 7 bits = 1111110x. High byte is 0xfc or 0xfd.
        if (first_byte & 0xfe) == 0xfc {
            return BaseUrlClassification::Private;
        }
        return BaseUrlClassification::Public;
    }

    // Not localhost, not the metadata hostname, not an IP literal —
    // must be a DNS hostname. Treat as public regardless of how it
    // spells. This is the security-relevant branch:
    // `127.0.0.1.evil.com`, `fcfoo.com`, `10.example.com` all land
    // here.
    BaseUrlClassification::Public
}

/// Strict IPv4 dotted-decimal parser. Mirrors the TS frontend's
/// `parseIPv4` — rejects leading zeros on multi-digit octets so
/// `010.0.0.1` (potentially octal) doesn't sneak past.
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    // Use the std parser as the inner check (catches range errors,
    // canonical decimal, etc.) but enforce our extra "no leading
    // zeros" rule on top. `Ipv4Addr` itself accepts `010.0.0.1` —
    // the textbook security advice from RFC 6943 is to reject those
    // because some downstream resolvers interpret the leading-zero
    // octet as octal.
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    for p in &parts {
        if p.is_empty() || p.len() > 3 {
            return None;
        }
        if !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if p.len() > 1 && p.starts_with('0') {
            return None;
        }
    }
    s.parse::<Ipv4Addr>().ok().map(|a| a.octets())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_classifies_as_empty() {
        assert_eq!(classify_base_url(""), BaseUrlClassification::Empty);
        assert_eq!(classify_base_url("   "), BaseUrlClassification::Empty);
    }

    #[test]
    fn loopback_urls_classify_as_loopback() {
        for url in [
            "http://localhost",
            "http://localhost:8080/v1",
            "http://127.0.0.1",
            "http://127.5.6.7:8080/v1",
            "http://0.0.0.0",
            "http://[::1]:8080/v1",
        ] {
            assert_eq!(
                classify_base_url(url),
                BaseUrlClassification::Loopback,
                "{url}",
            );
        }
    }

    #[test]
    fn private_rfc1918_classifies_as_private() {
        for url in [
            "http://10.0.0.5",
            "http://192.168.1.1:8080",
            "http://172.16.0.1",
            "http://172.31.255.255",
            "http://[fc00::1]:8080/v1",
            "http://[fd00:abcd::1]/v1",
        ] {
            assert_eq!(
                classify_base_url(url),
                BaseUrlClassification::Private,
                "{url}",
            );
        }
    }

    #[test]
    fn cloud_metadata_endpoints_classify_as_metadata() {
        for url in [
            "http://169.254.169.254",
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.169.254:80/v1",
            "http://metadata.google.internal/computeMetadata/v1/",
        ] {
            assert_eq!(
                classify_base_url(url),
                BaseUrlClassification::Metadata,
                "{url}",
            );
        }
    }

    #[test]
    fn other_link_local_classifies_as_link_local() {
        for url in [
            "http://169.254.1.1",
            "http://169.254.42.42",
            "http://[fe80::1]:8080/v1",
            "http://[fe80::abcd]/v1",
        ] {
            assert_eq!(
                classify_base_url(url),
                BaseUrlClassification::LinkLocal,
                "{url}",
            );
        }
    }

    #[test]
    fn public_urls_classify_as_public() {
        for url in [
            "https://api.openai.com/v1",
            "https://api.anthropic.com/",
            "http://8.8.8.8",
            "http://[2001:4860:4860::8888]/v1",
            "https://172.32.0.1", // 172.32 is OUTSIDE the 16..=31 RFC1918 block
            "https://172.15.0.1", // 172.15 is OUTSIDE too
        ] {
            assert_eq!(
                classify_base_url(url),
                BaseUrlClassification::Public,
                "{url}",
            );
        }
    }

    /// R10/High-1 regression: a DNS hostname that looks like a private
    /// IP prefix must NOT classify as private/loopback. The renderer-
    /// side classifier had this bug with naive `startsWith('127.')`
    /// checks; the Rust mirror needs the same regression guard so the
    /// command boundary doesn't re-introduce the hole.
    #[test]
    fn fake_prefix_hostnames_classify_as_public() {
        for url in [
            "https://127.0.0.1.evil.com/",
            "https://10.example.com/",
            "https://192.168.1.1.attacker.example/",
            "https://fcfoo.com/",
            "https://localhost.evil.com/",
            "https://metadata.google.internal.evil.com/",
        ] {
            assert_eq!(
                classify_base_url(url),
                BaseUrlClassification::Public,
                "spoofed prefix hostname must classify as public, not loopback/private/metadata: {url}",
            );
        }
    }

    /// IPv4 octal-leading-zero shapes (e.g. `0177.0.0.1`) must NOT be
    /// accepted as IP literals by our parser, because some downstream
    /// resolvers treat `0177` as octal-127. Falling through to
    /// hostname classification is fine — they'll land in `Public`,
    /// which is the safe bucket.
    #[test]
    fn ipv4_with_leading_zero_octets_does_not_classify_as_ip() {
        // `010.0.0.1` would be octal-008-dot-0.0.1 in some parsers, so
        // we deliberately reject it — falls through to hostname →
        // public.
        assert_eq!(
            classify_base_url("http://010.0.0.1/"),
            BaseUrlClassification::Public,
        );
        assert_eq!(
            classify_base_url("http://0177.0.0.1/"),
            BaseUrlClassification::Public,
        );
    }

    /// Pre-URL fallback: operator typed `localhost:1234` without a
    /// scheme. Classifier should still recognise the host.
    #[test]
    fn pre_scheme_inputs_still_classify() {
        assert_eq!(
            classify_base_url("localhost:1234"),
            BaseUrlClassification::Loopback,
        );
        assert_eq!(
            classify_base_url("127.0.0.1:8080"),
            BaseUrlClassification::Loopback,
        );
        assert_eq!(
            classify_base_url("169.254.169.254"),
            BaseUrlClassification::Metadata,
        );
    }

    #[test]
    fn malformed_ipv6_brackets_classify_as_invalid() {
        // Open bracket with no close — broken URL, refuse to guess.
        assert_eq!(
            classify_base_url("http://[::1"),
            BaseUrlClassification::Invalid,
        );
    }
}
