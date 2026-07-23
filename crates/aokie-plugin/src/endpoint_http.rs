//! Hardened outbound HTTP for operator-configured AI/STT/TTS endpoints (audit
//! AOK-ENDPOINT-001). The textual classification at settings.set time (PRIV-001,
//! `aokie_core::url_classification`) rejects blatant metadata/link-local URLs, but it
//! cannot close two runtime escape hatches:
//!
//!   1. REDIRECTS — reqwest follows up to 10 by default, so a "public HTTPS" endpoint
//!      could 302 a request carrying caller transcripts/audio to a private, link-local
//!      or cloud-metadata target. Every client built here uses `redirect::Policy::none()`;
//!      a redirecting endpoint surfaces as a plain HTTP error.
//!   2. DNS REBINDING — a public HOSTNAME's A/AAAA records are attacker-controllable and
//!      can point at 169.254.169.254 or the LAN at resolve time (or flip between checks).
//!      Hostname endpoints are resolved HERE, every resolved address must be a public
//!      unicast address, and the validated address is PINNED onto the client
//!      (`ClientBuilder::resolve`) so the connection can only go where we checked.
//!
//! IP-literal and `localhost` endpoints skip resolution/pinning — the operator typed the
//! address, the settings classifier already vetted its class, and loopback/LAN use is a
//! deliberate, warned-about deployment mode.
//!
//! Clients are cached per (endpoint origin, timeouts): reqwest clients are cheap to clone
//! (Arc inside), and caching keeps the pinned resolution stable for a call instead of
//! re-resolving per utterance.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Cached pinned client + when its DNS resolution happened (audit AK-13).
struct CachedClient {
    client: reqwest::blocking::Client,
    resolved_at: std::time::Instant,
}

/// How long one pinned resolution stays live (audit AK-13). Within the TTL a
/// call keeps its stable, validated address (the whole point of pinning); after
/// it, the next request re-resolves and re-validates so legitimate DNS
/// rotation/failover is honoured without a process restart. Five minutes
/// comfortably covers a call while still tracking endpoint DNS.
const CLIENT_TTL: Duration = Duration::from_secs(300);

static CACHE: OnceLock<Mutex<HashMap<(String, u64, u64), CachedClient>>> = OnceLock::new();

/// True only for addresses that are legitimately "the public internet".
///
/// Audit AK-06: delegates to the ONE IANA-based classifier in
/// `aokie_core::url_classification` — the same verdict the settings-time
/// classification uses, so the two policies can never drift. That shared
/// classifier also decodes translation/transition forms (IPv4-mapped, NAT64
/// `64:ff9b::/96`, 6to4 `2002::/16`) and judges the EMBEDDED IPv4, and
/// rejects Teredo, documentation, ORCHID, benchmarking, CGNAT, and the other
/// special-purpose ranges outright.
pub fn ip_is_public_unicast(ip: IpAddr) -> bool {
    aokie_core::url_classification::ip_is_global_unicast(ip)
}

/// Build (or fetch the cached) hardened client for `endpoint`. `Err` means the endpoint
/// must NOT receive the request — the message says why, and callers surface it through
/// their existing error paths (a failed speech/LLM request), never by falling back to an
/// unvalidated client.
pub fn client_for(
    endpoint: &str,
    timeout: Duration,
    connect_timeout: Option<Duration>,
) -> Result<reqwest::blocking::Client, String> {
    let parsed = aokie_core::url_classification::parse_base_url(endpoint)?;
    let url = parsed.url();
    let origin = parsed.canonical_origin().to_string();
    let key = (
        origin,
        timeout.as_millis() as u64,
        connect_timeout.map(|c| c.as_millis() as u64).unwrap_or(0),
    );
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut map) = cache.lock() {
        match map.get(&key) {
            // Fresh pin: keep the validated address stable (one call, one address).
            Some(cached) if cached.resolved_at.elapsed() < CLIENT_TTL => {
                return Ok(cached.client.clone());
            }
            // Expired pin (audit AK-13): drop it so the rebuild below re-resolves
            // and re-validates — endpoint DNS rotation/failover is honoured
            // instead of being pinned for the process lifetime.
            Some(_) => {
                map.remove(&key);
            }
            None => {}
        }
    }

    let mut builder = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout);
    if let Some(connect) = connect_timeout {
        builder = builder.connect_timeout(connect);
    }

    let host = url
        .host_str()
        .ok_or_else(|| "endpoint URL has no host".to_string())?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !ip.is_loopback() && !ip_is_public_unicast(ip) {
            return Err(format!(
                "endpoint IP {ip} is private, link-local, metadata, or otherwise non-public"
            ));
        }
    }

    // Hostname endpoints: resolve, validate every address, pin the first valid one.
    // Url::domain() is Some only for real domain hosts — IP literals return None and
    // (with `localhost`) skip resolution/pinning: vetted at settings time, on-device use.
    if let Some(host) = url
        .domain()
        .filter(|h| !h.eq_ignore_ascii_case("localhost"))
    {
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|e| format!("endpoint host '{host}' did not resolve: {e}"))?
            .collect();
        if addrs.is_empty() {
            return Err(format!("endpoint host '{host}' resolved to no addresses"));
        }
        if let Some(bad) = addrs.iter().find(|a| !ip_is_public_unicast(a.ip())) {
            return Err(format!(
                "endpoint host '{host}' resolves to non-public address {} — refusing to send caller data (possible DNS rebinding / SSRF)",
                bad.ip()
            ));
        }
        // Pin: the connection may only go to the address we just validated.
        // (reqwest ignores the port in the override; the URL's port applies.)
        builder = builder.resolve(host, SocketAddr::new(addrs[0].ip(), 0));
    }

    let client = builder
        .build()
        .map_err(|e| format!("could not build HTTP client: {e}"))?;
    if let Ok(mut map) = cache.lock() {
        // Bound the cache: evict expired pins first (audit AK-13), then fall
        // back to the wholesale clear so churn (tests, fuzzing) stays bounded.
        if map.len() > 32 {
            map.retain(|_, cached| cached.resolved_at.elapsed() < CLIENT_TTL);
            if map.len() > 32 {
                map.clear();
            }
        }
        map.insert(
            key,
            CachedClient {
                client: client.clone(),
                resolved_at: std::time::Instant::now(),
            },
        );
    }
    Ok(client)
}

/// True when `endpoint` targets the FormLogic desktop AI gateway — EXACTLY
/// host `127.0.0.1`, port `17872` (the desktop's loopback management/gateway
/// listener). Pure so the match rule is unit-testable; anything else (other
/// loopback ports, `localhost` spellings, LAN/public hosts) is NOT the
/// gateway and must never receive the gateway token.
pub fn is_formlogic_gateway(endpoint: &str) -> bool {
    let Ok(parsed) = aokie_core::url_classification::parse_base_url(endpoint) else {
        return false;
    };
    let url = parsed.url();
    url.host_str() == Some("127.0.0.1") && url.port_or_known_default() == Some(17_872)
}

/// The `Authorization: Bearer` token for the FormLogic AI gateway, read from
/// `FORMLOGIC_AI_GATEWAY_TOKEN` — returned ONLY when the endpoint is the
/// gateway itself ([`is_formlogic_gateway`]), so the token can never leak to
/// any other endpoint an operator configures.
pub fn gateway_bearer(endpoint: &str) -> Option<String> {
    if !is_formlogic_gateway(endpoint) {
        return None;
    }
    std::env::var("FORMLOGIC_AI_GATEWAY_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Attach the gateway bearer to a request when (and only when) its endpoint
/// is the FormLogic AI gateway. One call site per outbound speech/LLM request
/// keeps the rule uniform.
pub fn with_gateway_bearer(
    rb: reqwest::blocking::RequestBuilder,
    endpoint: &str,
) -> reqwest::blocking::RequestBuilder {
    match gateway_bearer(endpoint) {
        Some(token) => rb.bearer_auth(token),
        None => rb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn gateway_match_is_exact_host_and_port() {
        // The one shape that gets the token.
        assert!(is_formlogic_gateway(
            "http://127.0.0.1:17872/api/ai/v1/chat/completions"
        ));
        assert!(is_formlogic_gateway("http://127.0.0.1:17872/api/ai/v1"));
        // Everything else must NOT match — other ports, other loopback
        // spellings, remote hosts, a look-alike path on a foreign host,
        // missing/default ports, garbage.
        for not_gateway in [
            "http://127.0.0.1:8080/v1/chat/completions",
            "http://127.0.0.1/api/ai/v1/chat/completions", // default port 80
            "http://localhost:17872/api/ai/v1/chat/completions",
            "http://192.168.1.5:17872/api/ai/v1/chat/completions",
            "https://api.example.com:17872/api/ai/v1/chat/completions",
            "https://api.example.com/v1/chat/completions",
            "http://[::1]:17872/api/ai/v1/chat/completions",
            "not a url",
            "",
        ] {
            assert!(!is_formlogic_gateway(not_gateway), "{not_gateway}");
        }
    }

    #[test]
    fn public_unicast_gate_rejects_every_internal_class() {
        for bad in [
            "127.0.0.1",              // loopback
            "10.1.2.3",               // RFC1918
            "192.168.1.10",           // RFC1918
            "172.16.0.9",             // RFC1918
            "169.254.169.254",        // cloud metadata (link-local)
            "169.254.7.9",            // link-local
            "100.64.0.1",             // CGNAT
            "224.0.0.1",              // multicast
            "255.255.255.255",        // broadcast
            "0.0.0.0",                // unspecified
            "::1",                    // v6 loopback
            "fe80::1",                // v6 link-local
            "fc00::1",                // v6 unique-local
            "ff02::1",                // v6 multicast
            "::",                     // v6 unspecified
            "::ffff:10.0.0.1",        // v4-mapped private
            "::ffff:169.254.169.254", // v4-mapped metadata
        ] {
            assert!(!ip_is_public_unicast(v4(bad)), "{bad} must be rejected");
        }
        for good in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(ip_is_public_unicast(v4(good)), "{good} must be accepted");
        }
    }

    #[test]
    fn redirects_are_disabled_and_ip_literals_build_without_resolution() {
        // 127.0.0.1 endpoint: builds (no DNS involved), redirect policy none is baked in —
        // proven behaviourally: a client to a redirecting endpoint returns the 3xx itself
        // (exercised in the live E2E; here we just prove construction succeeds).
        let c = client_for(
            "http://127.0.0.1:9/v1/chat/completions",
            Duration::from_secs(1),
            Some(Duration::from_millis(100)),
        );
        assert!(c.is_ok());
        // Cached: same origin+timeouts returns a clone, not a rebuild.
        let c2 = client_for(
            "http://127.0.0.1:9/other/path",
            Duration::from_secs(1),
            Some(Duration::from_millis(100)),
        );
        assert!(c2.is_ok());
    }

    #[test]
    fn unparseable_endpoints_are_refused() {
        assert!(client_for("not a url", Duration::from_secs(1), None).is_err());
        assert!(client_for("", Duration::from_secs(1), None).is_err());
    }
}
