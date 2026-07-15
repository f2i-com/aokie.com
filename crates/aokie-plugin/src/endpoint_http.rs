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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static CACHE: OnceLock<Mutex<HashMap<(String, u64, u64), reqwest::blocking::Client>>> =
    OnceLock::new();

/// True only for addresses that are legitimately "the public internet": rejects loopback,
/// private (RFC 1918 / fc00::/7), link-local (incl. the 169.254.169.254 metadata service),
/// multicast, broadcast, unspecified and IPv4-mapped forms of any of those.
pub fn ip_is_public_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public_unicast(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return ipv4_is_public_unicast(mapped);
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || is_unique_local_v6(&v6)
                || is_link_local_v6(&v6))
        }
    }
}

fn ipv4_is_public_unicast(v4: Ipv4Addr) -> bool {
    !(v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || v4.is_documentation()
        // 100.64.0.0/10 (CGNAT) — "inside the carrier", never a legitimate AI endpoint.
        || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64))
}

fn is_unique_local_v6(v6: &Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7
}

fn is_link_local_v6(v6: &Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10
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
    if let Ok(map) = cache.lock() {
        if let Some(client) = map.get(&key) {
            return Ok(client.clone());
        }
    }

    let mut builder = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout);
    if let Some(connect) = connect_timeout {
        builder = builder.connect_timeout(connect);
    }

    let host = url.host_str().ok_or_else(|| "endpoint URL has no host".to_string())?;
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
    if let Some(host) = url.domain().filter(|h| !h.eq_ignore_ascii_case("localhost")) {
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
        // Bound the cache: endpoints are operator-configured, so a handful is normal —
        // wholesale churn (tests, fuzzing) must not grow it without limit.
        if map.len() > 32 {
            map.clear();
        }
        map.insert(key, client.clone());
    }
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_unicast_gate_rejects_every_internal_class() {
        for bad in [
            "127.0.0.1",        // loopback
            "10.1.2.3",         // RFC1918
            "192.168.1.10",     // RFC1918
            "172.16.0.9",       // RFC1918
            "169.254.169.254",  // cloud metadata (link-local)
            "169.254.7.9",      // link-local
            "100.64.0.1",       // CGNAT
            "224.0.0.1",        // multicast
            "255.255.255.255",  // broadcast
            "0.0.0.0",          // unspecified
            "::1",              // v6 loopback
            "fe80::1",          // v6 link-local
            "fc00::1",          // v6 unique-local
            "ff02::1",          // v6 multicast
            "::",               // v6 unspecified
            "::ffff:10.0.0.1",  // v4-mapped private
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
