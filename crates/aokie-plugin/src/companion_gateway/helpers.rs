//! Validation and URL/identity helpers shared across the gateway.

#[allow(unused_imports)]
use super::*;

pub(super) fn set_status(
    status: &Arc<Mutex<GatewayStatusSnapshot>>,
    phase: GatewayConnectionPhase,
    reconnect_attempt: u32,
    last_error: Option<String>,
) {
    if let Ok(mut status) = status.lock() {
        status.connected = phase == GatewayConnectionPhase::Connected;
        status.phase = phase;
        status.reconnect_attempt = reconnect_attempt;
        status.last_error = last_error.map(|message| sanitize_status_message(&message));
        status.changed_at = aokie_core::events::now_iso8601();
    }
}

pub(super) fn validate_identity(value: &str, field: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(format!("privateBootstrap {field} is invalid"))
    }
}

pub(super) fn validate_bearer(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_LEASE_TOKEN_BYTES
        || value.chars().any(char::is_control)
    {
        Err("privateBootstrap accessToken is invalid".into())
    } else {
        Ok(())
    }
}

pub(super) fn normalize_gateway_url(raw: &str) -> Result<Url, String> {
    let mut url =
        Url::parse(raw).map_err(|_| "privateBootstrap gatewayUrl is invalid".to_string())?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(
            "privateBootstrap gatewayUrl must not contain credentials or a fragment".into(),
        );
    }
    let secure = url.scheme() == "wss";
    let debug_loopback = cfg!(debug_assertions)
        && url.scheme() == "ws"
        && url.host_str().is_some_and(is_loopback_host);
    let managed_beta_loopback = cfg!(feature = "managed-beta-driver")
        && url.scheme() == "ws"
        && url.host_str().is_some_and(is_numeric_loopback_host);
    if !secure && !debug_loopback && !managed_beta_loopback {
        return Err("privateBootstrap gatewayUrl must use wss (debug loopback may use ws)".into());
    }
    let path = url.path().trim_end_matches('/').to_string();
    if path.is_empty() {
        url.set_path("/v2/realtime");
    } else if path.ends_with("/v2/realtime") {
        url.set_path(&path);
    } else if path != "/v2/realtime" {
        let joined = format!("{path}/v2/realtime");
        url.set_path(&joined);
    }
    Ok(url)
}

/// Relay endpoints are ordinary HTTP resources, so they cannot share
/// [`normalize_gateway_url`]: that one forces `wss` and rewrites the path to
/// `/v2/realtime`, which would destroy a mailbox URL. The path here is
/// authoritative and preserved exactly as advertised.
///
/// ⚠️ The `http` exception exists because the current deployment serves
/// `http://formlogic.local` with its API on `http://api.formlogic.local`:
/// neither presents a certificate, so requiring `https` would make the hosted
/// relay unreachable on the very install it was built for. It means the
/// admission bearer rides plaintext over the LAN, which is why the exception
/// is confined to loopback / `.local` hosts on managed-beta builds.
pub(super) fn normalize_relay_url(raw: &str, label: &str) -> Result<Url, WorkerError> {
    let invalid =
        |detail: &str| WorkerError::rebootstrap(format!("Companion relay {label} {detail}"));
    let url = Url::parse(raw).map_err(|_| invalid("is not an absolute URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("must not contain credentials"));
    }
    if url.fragment().is_some() {
        return Err(invalid("must not contain a fragment"));
    }
    let secure = url.scheme() == "https";
    let local_plaintext = cfg!(feature = "managed-beta-driver")
        && url.scheme() == "http"
        && url.host_str().is_some_and(is_local_network_host);
    if !secure && !local_plaintext {
        return Err(invalid(
            "must use https (managed-beta builds may use http on a loopback or .local host)",
        ));
    }
    Ok(url)
}

/// Loopback plus the mDNS `.local` names the desktop install actually serves.
pub(super) fn is_local_network_host(host: &str) -> bool {
    is_loopback_host(host) || host.to_ascii_lowercase().ends_with(".local")
}

pub(super) fn is_numeric_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1"
}

pub(super) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host.ends_with(".localhost")
}

pub(super) fn redacted_url(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .map(|mut url| {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|| "[invalid URL]".into())
}

pub(super) fn sanitize_status_message(message: &str) -> String {
    let without_lines = message.lines().next().unwrap_or("Companion gateway error");
    without_lines.chars().take(240).collect()
}

pub(super) fn unix_now() -> Result<u64, WorkerError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| WorkerError::rebootstrap("System clock is before the Unix epoch"))
}

/// The relay's party identifier for a Companion endpoint key.
///
/// Lives here rather than in the voice-gated carrier so the session (which
/// compiles unconditionally) and [`crate::companion_relay`] cannot drift: the
/// carrier's approved-party set, its greeting book and the session's re-greet
/// signal must all name a party the same way or the greeting is retired for a
/// party that does not exist.
pub(crate) fn relay_party(endpoint_key_thumbprint: &str) -> String {
    format!("mobile:{endpoint_key_thumbprint}")
}

/// Whether this build may act as the lease authority on the relay carrier.
///
/// Set `AOKIE_RELAY_LEASE_AUTHORITY=0` to switch the whole path off on a
/// running install without a rebuild: no offers are published and every arm
/// beyond the authenticated hello goes back to being dropped, which is exactly
/// how the relay behaved before. Anything else, including the variable being
/// absent, leaves it on.
pub(super) fn relay_lease_authority_enabled() -> bool {
    !std::env::var("AOKIE_RELAY_LEASE_AUTHORITY").is_ok_and(|value| value.trim() == "0")
}

pub(super) fn relay_mode_grant(mode: LeaseMode) -> Grant {
    match mode {
        LeaseMode::Monitor => Grant::Monitor,
        LeaseMode::Consult => Grant::Consult,
        LeaseMode::Takeover => Grant::Takeover,
    }
}

/// Authority required for every relay media operation.
///
/// These grants come only from FormLogic's authenticated outer envelope or a
/// previously verified hello carrying that metadata. A frame's own `mode`,
/// `requiredGrants`, or any other peer-controlled field never supplies one.
pub(super) fn relay_grants_allow_mode(grants: &HashSet<Grant>, mode: LeaseMode) -> bool {
    grants.contains(&Grant::StateRead)
        && grants.contains(&Grant::RtcSignal)
        && grants.contains(&relay_mode_grant(mode))
        // A takeover claimant must already be authorized to return the
        // caller to Aokie.  Granting seizure without its mandatory failback
        // operation would turn later policy narrowing or media failure into
        // dead air. Exact-holder revoke remains identity-fenced separately.
        && (mode != LeaseMode::Takeover || grants.contains(&Grant::ResumeAokie))
}

/// Compare two bearer tokens without leaking their divergence point.
///
/// These are secrets the plugin minted and the peer presents back, so the
/// comparison is the authentication step; a short-circuiting `==` would let a
/// peer recover a token byte by byte from timing.
pub(super) fn tokens_match(minted: &str, presented: &str) -> bool {
    let minted = minted.as_bytes();
    let presented = presented.as_bytes();
    if minted.len() != presented.len() {
        return false;
    }
    minted
        .iter()
        .zip(presented)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// Whether the last emitted lease-status event still denotes live ACTIVE
/// authority.
///
/// `Renewed` is deliberately retained in the relay registry so an at-least-once
/// heartbeat can replay the current `lease_renewed` frame. It is a lifecycle
/// event, not a demotion: action gates must therefore accept both the initial
/// ACTIVE status and every delivered renewal while the signed lease phase and
/// the session lease book independently remain active.
pub(super) fn relay_status_has_active_authority(status: PluginLeaseStatus) -> bool {
    matches!(
        status,
        PluginLeaseStatus::Active | PluginLeaseStatus::Renewed
    )
}

pub(super) fn transfer_opportunity_id(request_id: &str) -> String {
    let digest = Sha256::digest(request_id.as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("transfer_opportunity_{suffix}")
}
