use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aokie_media::IceServerConfig;
use aokie_protocol::v2::EndpointPublicKey;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use reqwest::redirect::Policy;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use url::Url;

const MAX_DISCOVERY_BYTES: usize = 64 * 1024;
const MAX_SIGNING_KEY_BYTES: usize = 16 * 1024;
const MIN_TURN_CREDENTIAL_TTL_SECONDS: u64 = 30;
const MAX_TURN_CREDENTIAL_TTL_SECONDS: u64 = 24 * 60 * 60;

const V2_PAYLOAD_KEYS: &[&str] = &[
    "schemaVersion",
    "issuer",
    "apiBaseUrl",
    "gatewayUrl",
    "realtimeUrl",
    "oauthAuthorizationUrl",
    "oauthTokenUrl",
    "oauthResource",
    "admissionEndpoint",
    "clientId",
    "deploymentId",
    "available",
    "scopesSupported",
    "features",
    "remoteConsent",
    "iceServers",
    "relayOnly",
    "turnCredentialExpiresAt",
    "media",
    "appId",
    "appSlug",
];

const V2_TRUST_KEYS: &[&str] = &[
    "trustStatus",
    "signingKeyId",
    "signatureAlgorithm",
    "signature",
    "signingKeyUrl",
    "signatureEnvelope",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyDiscoveryDocument {
    schema_version: u16,
    issuer: String,
    api_base_url: String,
    realtime_url: String,
    oauth_authorization_url: String,
    oauth_token_url: String,
    deployment_id: String,
    signing_key_id: String,
    signature: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct V2SignedPayload {
    schema_version: u16,
    issuer: String,
    api_base_url: String,
    gateway_url: Option<String>,
    realtime_url: Option<String>,
    oauth_authorization_url: String,
    oauth_token_url: String,
    oauth_resource: String,
    admission_endpoint: String,
    client_id: String,
    deployment_id: String,
    available: bool,
    scopes_supported: Vec<String>,
    features: Vec<String>,
    remote_consent: RemoteConsentDiscovery,
    #[serde(default)]
    ice_servers: Vec<DiscoveryIceServer>,
    relay_only: bool,
    #[serde(deserialize_with = "deserialize_nullable_unix_timestamp")]
    turn_credential_expires_at: NullableUnixTimestamp,
    media: MediaDiscovery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_slug: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct NullableUnixTimestamp(Option<u64>);

impl NullableUnixTimestamp {
    pub(crate) fn value(self) -> Option<u64> {
        self.0
    }
}

impl From<Option<u64>> for NullableUnixTimestamp {
    fn from(value: Option<u64>) -> Self {
        Self(value)
    }
}

pub(crate) fn deserialize_nullable_unix_timestamp<'de, D>(
    deserializer: D,
) -> Result<NullableUnixTimestamp, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer).map(NullableUnixTimestamp)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteConsentDiscovery {
    configured: bool,
    remote_monitoring: bool,
    remote_consult: bool,
    remote_takeover: bool,
    remote_captions: bool,
    remote_assistance: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DiscoveryIceServer {
    urls: Vec<String>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    credential: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
}

impl DiscoveryIceServer {
    fn runtime_config(&self) -> IceServerConfig {
        IceServerConfig {
            urls: self.urls.clone(),
            username: self.username.clone(),
            credential: self.credential.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaDiscovery {
    transport: String,
    gateway_relays_media: bool,
    companion_uses_bluetooth_dongle: bool,
    relay_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrustFields {
    trust_status: String,
    signing_key_id: Option<String>,
    signature_algorithm: Option<String>,
    signature: Option<String>,
    signing_key_url: String,
    signature_envelope: Option<SignatureEnvelope>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignatureEnvelope {
    payload: Value,
    signature: String,
    alg: String,
    key_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SigningKeyDocument {
    alg: String,
    key_id: String,
    public_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryDocument {
    pub(crate) schema_version: u16,
    pub(crate) issuer: String,
    pub(crate) api_base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) gateway_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) realtime_url: Option<String>,
    pub(crate) oauth_authorization_url: String,
    pub(crate) oauth_token_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) oauth_resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) admission_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) client_id: Option<String>,
    pub(crate) deployment_id: String,
    pub(crate) available: bool,
    pub(crate) scopes_supported: Vec<String>,
    pub(crate) features: Vec<String>,
    pub(crate) ice_servers: Vec<IceServerConfig>,
    pub(crate) relay_only: bool,
    pub(crate) turn_credential_expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remote_consent: Option<RemoteConsentDiscovery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    media: Option<MediaDiscovery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) app_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) app_slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signing_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) signing_key_fingerprint: Option<String>,
    pub(crate) signature_verified: bool,
}

#[cfg(test)]
pub(crate) fn test_discovery_document_with_relay_policy(relay_only: bool) -> DiscoveryDocument {
    DiscoveryDocument {
        schema_version: 2,
        issuer: "https://formlogic.example".into(),
        api_base_url: "https://formlogic.example/api".into(),
        gateway_url: Some("wss://formlogic.example/v2/realtime".into()),
        realtime_url: Some("wss://formlogic.example/v2/realtime".into()),
        oauth_authorization_url: "https://formlogic.example/oauth/authorize".into(),
        oauth_token_url: "https://formlogic.example/api/oauth/token".into(),
        oauth_resource: Some("https://formlogic.example/api/aokie-companion".into()),
        admission_endpoint: Some("https://formlogic.example/api/aokie-companion/admission".into()),
        client_id: Some("aokie-companion".into()),
        deployment_id: "deployment_1".into(),
        available: true,
        scopes_supported: vec!["aokie:state".into(), "offline_access".into()],
        features: vec!["state".into()],
        ice_servers: Vec::new(),
        relay_only,
        turn_credential_expires_at: None,
        remote_consent: Some(RemoteConsentDiscovery {
            configured: false,
            remote_monitoring: false,
            remote_consult: false,
            remote_takeover: false,
            remote_captions: false,
            remote_assistance: false,
        }),
        media: Some(MediaDiscovery {
            transport: "webrtc".into(),
            gateway_relays_media: false,
            companion_uses_bluetooth_dongle: false,
            relay_only,
        }),
        app_id: Some("app_1".into()),
        app_slug: Some("app-1".into()),
        signing_key_id: Some("key_1".into()),
        signing_key_fingerprint: Some("fingerprint_1".into()),
        signature_verified: true,
    }
}

fn is_loopback(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}

fn is_numeric_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
}

fn is_formlogic_debug_origin(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("formlogic.local" | "api.formlogic.local")
    )
}

fn is_formlogic_local_api_origin(url: &Url) -> bool {
    url.host_str() == Some("api.formlogic.local")
}

fn validate_url_scheme(
    value: &str,
    label: &str,
    secure_scheme: &str,
    debug_loopback_scheme: &str,
) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| format!("{label} is not a valid URL"))?;
    let secure = url.scheme() == secure_scheme;
    let debug_local_wamp = debug_loopback_scheme == "http" && is_formlogic_debug_origin(&url);
    let debug_loopback = cfg!(debug_assertions)
        && (is_loopback(&url) || debug_local_wamp)
        && url.scheme() == debug_loopback_scheme;
    let managed_beta_local = cfg!(feature = "managed-beta-local")
        && url.scheme() == debug_loopback_scheme
        && if debug_loopback_scheme == "http" {
            is_formlogic_local_api_origin(&url) || is_numeric_loopback(&url)
        } else {
            is_numeric_loopback(&url)
        };
    if !secure && !debug_loopback && !managed_beta_local {
        return Err(format!("{label} must use {secure_scheme}"));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(format!("{label} contains unsupported URL components"));
    }
    Ok(url)
}

fn validate_https_url(value: &str, label: &str) -> Result<Url, String> {
    validate_url_scheme(value, label, "https", "http")
}

fn validate_wss_url(value: &str, label: &str) -> Result<Url, String> {
    validate_url_scheme(value, label, "wss", "ws")
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn append_chunk(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    maximum: usize,
    label: &str,
) -> Result<(), String> {
    if chunk.len() > maximum.saturating_sub(buffer.len()) {
        return Err(format!("{label} is too large"));
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}

#[cfg(test)]
fn append_discovery_chunk(buffer: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    append_chunk(buffer, chunk, MAX_DISCOVERY_BYTES, "discovery document")
}

fn safe_text(value: &str, maximum: usize, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(format!("{label} is invalid"))
    } else {
        Ok(())
    }
}

fn safe_id(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(format!("{label} is invalid"))
    } else {
        Ok(())
    }
}

fn validate_unique_text(
    values: &[String],
    maximum_items: usize,
    label: &str,
) -> Result<(), String> {
    if values.is_empty() || values.len() > maximum_items {
        return Err(format!("{label} is invalid"));
    }
    let mut unique = HashSet::new();
    for value in values {
        safe_text(value, 100, label)?;
        if !unique.insert(value) {
            return Err(format!("{label} contains duplicates"));
        }
    }
    Ok(())
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before the Unix epoch".to_string())
}

fn has_feature(features: &[String], feature: &str) -> bool {
    features.iter().any(|candidate| candidate == feature)
}

fn validate_remote_consent(payload: &V2SignedPayload) -> Result<(), String> {
    let consent = &payload.remote_consent;
    if !consent.configured
        && (consent.remote_monitoring
            || consent.remote_consult
            || consent.remote_takeover
            || consent.remote_captions
            || consent.remote_assistance)
    {
        return Err("unconfigured remoteConsent must fail closed".into());
    }

    for (feature, enabled) in [
        ("monitor", consent.remote_monitoring),
        ("consult", consent.remote_consult),
        ("captions", consent.remote_captions),
        ("typed_assistance", consent.remote_assistance),
        ("takeover", consent.remote_takeover),
        ("return_to_aokie", consent.remote_takeover),
    ] {
        if has_feature(&payload.features, feature) != enabled {
            return Err(format!(
                "remoteConsent and the signed {feature} feature disagree"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_managed_ice_configuration(
    servers: &[DiscoveryIceServer],
    relay_only: bool,
    turn_credential_expires_at: Option<u64>,
    now: u64,
) -> Result<Vec<IceServerConfig>, String> {
    let runtime_servers: Vec<_> = servers
        .iter()
        .map(DiscoveryIceServer::runtime_config)
        .collect();
    IceServerConfig::validate_all(&runtime_servers).map_err(|error| error.to_string())?;

    let mut earliest_turn_expiry: Option<u64> = None;
    for server in servers {
        let has_turn = server.urls.iter().any(|url| {
            let lower = url.to_ascii_lowercase();
            lower.starts_with("turn:") || lower.starts_with("turns:")
        });
        if has_turn {
            if server.username.is_empty() || server.credential.is_empty() {
                return Err("TURN servers require short-lived credentials".into());
            }
            let expiry = server.expires_at.ok_or("TURN servers require expiresAt")?;
            if expiry <= now.saturating_add(MIN_TURN_CREDENTIAL_TTL_SECONDS)
                || expiry > now.saturating_add(MAX_TURN_CREDENTIAL_TTL_SECONDS)
            {
                return Err("TURN expiresAt must be 31 seconds to 24 hours in the future".into());
            }
            earliest_turn_expiry = Some(
                earliest_turn_expiry
                    .map(|current| current.min(expiry))
                    .unwrap_or(expiry),
            );
        } else if !server.username.is_empty()
            || !server.credential.is_empty()
            || server.expires_at.is_some()
        {
            return Err("STUN-only entries cannot contain credentials or expiresAt".into());
        }
    }

    if relay_only && earliest_turn_expiry.is_none() {
        return Err("relayOnly requires at least one TURN server".into());
    }
    if earliest_turn_expiry != turn_credential_expires_at {
        return Err("turnCredentialExpiresAt does not match the earliest TURN expiry".into());
    }
    Ok(runtime_servers)
}

fn split_v2_document(value: Value) -> Result<(Value, TrustFields), String> {
    let object = value
        .as_object()
        .ok_or("discovery document must be an object")?;
    if object.keys().any(|key| {
        !V2_PAYLOAD_KEYS.contains(&key.as_str()) && !V2_TRUST_KEYS.contains(&key.as_str())
    }) {
        return Err("schema-v2 discovery contains an unknown field".into());
    }
    let mut payload = Map::new();
    let mut trust = Map::new();
    for (key, value) in object {
        if V2_PAYLOAD_KEYS.contains(&key.as_str()) {
            payload.insert(key.clone(), value.clone());
        } else {
            trust.insert(key.clone(), value.clone());
        }
    }
    let trust = serde_json::from_value(Value::Object(trust))
        .map_err(|_| "schema-v2 discovery trust metadata is invalid".to_string())?;
    Ok((Value::Object(payload), trust))
}

fn select_v2_gateway(payload: &V2SignedPayload) -> Result<Option<String>, String> {
    let selected = match (
        payload.gateway_url.as_deref(),
        payload.realtime_url.as_deref(),
    ) {
        (Some(gateway), Some(legacy)) if gateway != legacy => {
            return Err("gatewayUrl and realtimeUrl aliases disagree".into());
        }
        (Some(gateway), _) => Some(gateway),
        (None, Some(legacy)) => Some(legacy),
        (None, None) => None,
    };
    if payload.available && selected.is_none() {
        return Err("available deployment omitted gatewayUrl".into());
    }
    if !payload.available && selected.is_some() {
        return Err("unavailable deployment unexpectedly advertises gatewayUrl".into());
    }
    if let Some(selected) = selected {
        let parsed = validate_wss_url(selected, "gatewayUrl")?;
        if !parsed.path().ends_with("/v2/realtime") {
            return Err("gatewayUrl must target /v2/realtime".into());
        }
    }
    Ok(selected.map(str::to_owned))
}

fn validate_v2_payload(
    payload: &V2SignedPayload,
    discovery_url: &Url,
    signing_key_url: &str,
) -> Result<Option<String>, String> {
    if payload.schema_version != 2 {
        return Err(format!(
            "unsupported discovery schema {}",
            payload.schema_version
        ));
    }
    safe_text(&payload.issuer, 500, "issuer")?;
    safe_id(&payload.deployment_id, "deploymentId")?;
    safe_id(&payload.client_id, "clientId")?;
    if let Some(app_id) = &payload.app_id {
        safe_id(app_id, "appId")?;
    }
    if let Some(app_slug) = &payload.app_slug {
        safe_id(app_slug, "appSlug")?;
    }
    validate_unique_text(&payload.scopes_supported, 32, "scopesSupported")?;
    validate_unique_text(&payload.features, 32, "features")?;
    if !payload
        .scopes_supported
        .iter()
        .any(|scope| scope == "aokie:state")
    {
        return Err("scopesSupported omitted aokie:state".into());
    }
    validate_remote_consent(payload)?;
    if payload.media.relay_only != payload.relay_only {
        return Err("top-level and media relayOnly fields disagree".into());
    }
    validate_managed_ice_configuration(
        &payload.ice_servers,
        payload.relay_only,
        payload.turn_credential_expires_at.value(),
        unix_now()?,
    )?;
    if payload.media.transport != "webrtc"
        || payload.media.gateway_relays_media
        || payload.media.companion_uses_bluetooth_dongle
    {
        return Err("discovery media topology is unsupported or unsafe".into());
    }

    let issuer = validate_https_url(&payload.issuer, "issuer")?;
    if issuer.path() != "/" || issuer.query().is_some() || !same_origin(&issuer, discovery_url) {
        return Err("issuer must be the discovery endpoint origin".into());
    }
    for (value, label) in [
        (&payload.api_base_url, "apiBaseUrl"),
        (&payload.oauth_authorization_url, "oauthAuthorizationUrl"),
        (&payload.oauth_token_url, "oauthTokenUrl"),
        (&payload.oauth_resource, "oauthResource"),
        (&payload.admission_endpoint, "admissionEndpoint"),
        (&signing_key_url.to_owned(), "signingKeyUrl"),
    ] {
        let parsed = validate_https_url(value, label)?;
        if !same_origin(&issuer, &parsed) {
            return Err(format!("{label} must share the trusted issuer origin"));
        }
    }
    select_v2_gateway(payload)
}

async fn read_bounded_response(
    mut response: reqwest::Response,
    maximum: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    if !response.status().is_success() {
        return Err(format!("{label} returned HTTP {}", response.status()));
    }
    if response.content_length().unwrap_or(0) > maximum as u64 {
        return Err(format!("{label} is too large"));
    }
    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(maximum as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| format!("could not read {label}"))?
    {
        append_chunk(&mut bytes, &chunk, maximum, label)?;
    }
    Ok(bytes)
}

async fn verify_v2_signature(
    client: &reqwest::Client,
    top_payload: &Value,
    trust: &TrustFields,
    discovery_url: &Url,
) -> Result<Option<String>, String> {
    if trust.trust_status == "unverified" {
        if trust.signing_key_id.is_some()
            || trust.signature_algorithm.is_some()
            || trust.signature.is_some()
            || trust.signature_envelope.is_some()
        {
            return Err("unverified discovery contains partial signature metadata".into());
        }
        return Ok(None);
    }
    if trust.trust_status != "signed" {
        return Err("discovery trustStatus is unsupported".into());
    }
    let envelope = trust
        .signature_envelope
        .as_ref()
        .ok_or("signed discovery omitted signatureEnvelope")?;
    let key_id = trust
        .signing_key_id
        .as_deref()
        .ok_or("signed discovery omitted signingKeyId")?;
    let algorithm = trust
        .signature_algorithm
        .as_deref()
        .ok_or("signed discovery omitted signatureAlgorithm")?;
    let signature = trust
        .signature
        .as_deref()
        .ok_or("signed discovery omitted signature")?;
    safe_id(key_id, "signingKeyId")?;
    if algorithm != "Ed25519"
        || envelope.alg != algorithm
        || envelope.key_id != key_id
        || envelope.signature != signature
        || &envelope.payload != top_payload
    {
        return Err("signatureEnvelope does not exactly bind the discovery payload".into());
    }

    let signing_url = validate_https_url(&trust.signing_key_url, "signingKeyUrl")?;
    if !same_origin(discovery_url, &signing_url) {
        return Err("signingKeyUrl must share the discovery origin".into());
    }
    let response = client
        .get(signing_url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| "could not reach signingKeyUrl".to_string())?;
    let bytes =
        read_bounded_response(response, MAX_SIGNING_KEY_BYTES, "signing key document").await?;
    let key: SigningKeyDocument = serde_json::from_slice(&bytes)
        .map_err(|_| "signing key document is invalid".to_string())?;
    if key.alg != "Ed25519" || key.key_id != key_id {
        return Err("signing key identity does not match the discovery envelope".into());
    }
    let public_key = key.public_key.ok_or("signing key omitted publicKey")?;
    let public_key = STANDARD
        .decode(public_key)
        .map_err(|_| "signing public key is not valid base64")?;
    let public_key: [u8; 32] = public_key
        .try_into()
        .map_err(|_| "signing public key must contain 32 bytes")?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|_| "signing public key is not a valid Ed25519 key")?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| "discovery signature is not valid base64url")?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| "discovery signature must contain 64 bytes")?;
    // serde_json's preserve_order feature retains the exact object insertion
    // order received in signatureEnvelope.payload. PHP signs the same compact,
    // unescaped JSON representation.
    let message = serde_json::to_vec(&envelope.payload)
        .map_err(|_| "could not canonicalize the signed discovery payload")?;
    verifying_key
        .verify(&message, &signature)
        .map_err(|_| "discovery signature verification failed")?;
    Ok(Some(
        EndpointPublicKey::from_ed25519_bytes(&public_key).thumbprint,
    ))
}

fn legacy_document(document: LegacyDiscoveryDocument) -> Result<DiscoveryDocument, String> {
    if document.schema_version != 1 {
        return Err(format!(
            "unsupported discovery schema {}",
            document.schema_version
        ));
    }
    safe_text(&document.issuer, 500, "issuer")?;
    safe_id(&document.deployment_id, "deploymentId")?;
    safe_id(&document.signing_key_id, "signingKeyId")?;
    safe_text(&document.signature, 4_096, "signature")?;
    validate_https_url(&document.api_base_url, "apiBaseUrl")?;
    validate_https_url(&document.oauth_authorization_url, "oauthAuthorizationUrl")?;
    validate_https_url(&document.oauth_token_url, "oauthTokenUrl")?;
    let gateway = validate_wss_url(&document.realtime_url, "realtimeUrl")?;
    if !gateway.path().ends_with("/v1/realtime") {
        return Err("realtimeUrl must target /v1/realtime".into());
    }
    Ok(DiscoveryDocument {
        schema_version: 1,
        issuer: document.issuer,
        api_base_url: document.api_base_url,
        gateway_url: Some(document.realtime_url.clone()),
        realtime_url: Some(document.realtime_url),
        oauth_authorization_url: document.oauth_authorization_url,
        oauth_token_url: document.oauth_token_url,
        oauth_resource: None,
        admission_endpoint: None,
        client_id: None,
        deployment_id: document.deployment_id,
        available: true,
        scopes_supported: Vec::new(),
        features: Vec::new(),
        ice_servers: Vec::new(),
        relay_only: false,
        turn_credential_expires_at: None,
        remote_consent: None,
        media: None,
        app_id: None,
        app_slug: None,
        signing_key_id: Some(document.signing_key_id),
        signing_key_fingerprint: None,
        // Legacy signatures did not have an unambiguous signed payload or
        // native-verifiable key endpoint and therefore remain untrusted.
        signature_verified: false,
    })
}

/// Fetches, structurally validates and (for schema v2) cryptographically
/// verifies deployment discovery using native TLS. Redirects are forbidden.
#[tauri::command]
pub async fn discover_deployment(url: String) -> Result<DiscoveryDocument, String> {
    fetch_discovery(&url).await
}

pub(crate) async fn fetch_discovery(url: &str) -> Result<DiscoveryDocument, String> {
    let parsed = validate_https_url(url, "discovery URL")?;
    let discovery_path = parsed.path();
    if !discovery_path.ends_with("aokie-discovery")
        && !discovery_path.contains("/.well-known/")
        && !cfg!(debug_assertions)
    {
        return Err("discovery URL must target the Aokie discovery document".into());
    }

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|_| "could not initialise native TLS".to_string())?;
    let response = client
        .get(parsed.clone())
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| "could not reach the discovery endpoint".to_string())?;
    let bytes = read_bounded_response(response, MAX_DISCOVERY_BYTES, "discovery document").await?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "discovery endpoint returned invalid JSON".to_string())?;
    let schema = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .ok_or("discovery document omitted schemaVersion")?;
    if schema == 1 {
        let document: LegacyDiscoveryDocument = serde_json::from_value(value)
            .map_err(|_| "discovery document does not match schema v1".to_string())?;
        return legacy_document(document);
    }
    if schema != 2 {
        return Err(format!("unsupported discovery schema {schema}"));
    }

    let (payload_value, trust) = split_v2_document(value)?;
    let payload: V2SignedPayload = serde_json::from_value(payload_value.clone())
        .map_err(|_| "discovery document does not match schema v2".to_string())?;
    let gateway_url = validate_v2_payload(&payload, &parsed, &trust.signing_key_url)?;
    let signing_key_fingerprint =
        verify_v2_signature(&client, &payload_value, &trust, &parsed).await?;
    let ice_servers = payload
        .ice_servers
        .iter()
        .map(DiscoveryIceServer::runtime_config)
        .collect();
    Ok(DiscoveryDocument {
        schema_version: 2,
        issuer: payload.issuer,
        api_base_url: payload.api_base_url,
        gateway_url: gateway_url.clone(),
        realtime_url: gateway_url,
        oauth_authorization_url: payload.oauth_authorization_url,
        oauth_token_url: payload.oauth_token_url,
        oauth_resource: Some(payload.oauth_resource),
        admission_endpoint: Some(payload.admission_endpoint),
        client_id: Some(payload.client_id),
        deployment_id: payload.deployment_id,
        available: payload.available,
        scopes_supported: payload.scopes_supported,
        features: payload.features,
        ice_servers,
        relay_only: payload.relay_only,
        turn_credential_expires_at: payload.turn_credential_expires_at.value(),
        remote_consent: Some(payload.remote_consent),
        media: Some(payload.media),
        app_id: payload.app_id,
        app_slug: payload.app_slug,
        signing_key_id: trust.signing_key_id,
        signature_verified: signing_key_fingerprint.is_some(),
        signing_key_fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn url_fields_are_scheme_and_origin_bound() {
        assert!(validate_https_url("https://api.example/aokie-discovery", "url").is_ok());
        assert!(validate_https_url("wss://api.example/aokie-discovery", "url").is_err());
        assert!(validate_wss_url("wss://realtime.example/v2/realtime", "url").is_ok());
        assert!(validate_wss_url("https://realtime.example/v2/realtime", "url").is_err());
        let first = Url::parse("https://example.test/path").unwrap();
        let second = Url::parse("https://example.test:443/other").unwrap();
        assert!(same_origin(&first, &second));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_loopback_exceptions_are_field_specific() {
        assert!(validate_https_url("http://127.0.0.1:3000/aokie-discovery", "url").is_ok());
        assert!(
            validate_https_url("http://formlogic.local/.well-known/aokie-companion", "url").is_ok()
        );
        assert!(validate_https_url(
            "http://api.formlogic.local/.well-known/aokie-companion",
            "url"
        )
        .is_ok());
        assert!(validate_https_url("http://evil.formlogic.local/aokie-discovery", "url").is_err());
        assert!(validate_https_url("ws://127.0.0.1:3000/aokie-discovery", "url").is_err());
        assert!(validate_wss_url("ws://localhost:3001/v2/realtime", "url").is_ok());
        assert!(validate_wss_url("ws://formlogic.local/v2/realtime", "url").is_err());
    }

    #[test]
    fn managed_local_http_origin_is_the_exact_api_host() {
        assert!(is_formlogic_local_api_origin(
            &Url::parse("http://api.formlogic.local/aokie-discovery").unwrap()
        ));
        assert!(!is_formlogic_local_api_origin(
            &Url::parse("http://formlogic.local/aokie-discovery").unwrap()
        ));
        assert!(!is_formlogic_local_api_origin(
            &Url::parse("http://evil.api.formlogic.local/aokie-discovery").unwrap()
        ));
    }

    #[test]
    fn cumulative_body_limit_is_enforced_across_chunks() {
        let mut body = Vec::new();
        append_discovery_chunk(&mut body, &vec![0; MAX_DISCOVERY_BYTES - 1]).unwrap();
        append_discovery_chunk(&mut body, &[0]).unwrap();
        assert_eq!(body.len(), MAX_DISCOVERY_BYTES);
        assert!(append_discovery_chunk(&mut body, &[0]).is_err());
    }

    #[test]
    fn v2_split_rejects_unknown_fields_and_binds_envelope_payload() {
        let payload = json!({
            "schemaVersion":2,
            "issuer":"https://formlogic.example",
            "apiBaseUrl":"https://formlogic.example/api",
            "gatewayUrl":"wss://gateway.example/v2/realtime",
            "realtimeUrl":"wss://gateway.example/v2/realtime",
            "oauthAuthorizationUrl":"https://formlogic.example/oauth/authorize",
            "oauthTokenUrl":"https://formlogic.example/api/oauth/token",
            "oauthResource":"https://formlogic.example/api/aokie-companion",
            "admissionEndpoint":"https://formlogic.example/api/aokie-companion/admission",
            "clientId":"aokie-companion",
            "deploymentId":"deployment_a",
            "available":true,
            "scopesSupported":["aokie:state","offline_access"],
            "features":["captions"],
            "remoteConsent":{"configured":true,"remoteMonitoring":false,"remoteConsult":false,"remoteTakeover":false,"remoteCaptions":true,"remoteAssistance":false},
            "iceServers":[],
            "relayOnly":false,
            "turnCredentialExpiresAt":null,
            "media":{"transport":"webrtc","gatewayRelaysMedia":false,"companionUsesBluetoothDongle":false,"relayOnly":false}
        });
        let mut document = payload.as_object().unwrap().clone();
        document.insert("trustStatus".into(), json!("signed"));
        document.insert("signingKeyId".into(), json!("key_a"));
        document.insert("signatureAlgorithm".into(), json!("Ed25519"));
        document.insert("signature".into(), json!("sig"));
        document.insert(
            "signingKeyUrl".into(),
            json!("https://formlogic.example/api/public/signing-key"),
        );
        document.insert(
            "signatureEnvelope".into(),
            json!({"payload":payload,"signature":"sig","alg":"Ed25519","keyId":"key_a"}),
        );
        let (split, trust) = split_v2_document(Value::Object(document.clone())).unwrap();
        assert_eq!(trust.signature_envelope.unwrap().payload, split);

        document.insert("unexpected".into(), json!(true));
        assert!(split_v2_document(Value::Object(document)).is_err());
    }

    #[test]
    fn current_formlogic_schema_v2_shape_is_accepted_without_opening_the_schema() {
        let payload = json!({
            "schemaVersion":2,
            "issuer":"http://formlogic.local",
            "apiBaseUrl":"http://formlogic.local/api",
            "gatewayUrl":"ws://127.0.0.1:18787/v2/realtime",
            "realtimeUrl":"ws://127.0.0.1:18787/v2/realtime",
            "oauthAuthorizationUrl":"http://formlogic.local/oauth/authorize",
            "oauthTokenUrl":"http://formlogic.local/api/oauth/token",
            "oauthResource":"http://formlogic.local/api/aokie-companion",
            "admissionEndpoint":"http://formlogic.local/api/aokie-companion/admission",
            "clientId":"aokie-companion",
            "deploymentId":"formlogic-local",
            "available":true,
            "scopesSupported":["aokie:state","aokie:monitor","aokie:consult","aokie:takeover","aokie:resume","aokie:assistance","aokie:end_caller","offline_access"],
            "features":["state"],
            "remoteConsent":{"configured":false,"remoteMonitoring":false,"remoteConsult":false,"remoteTakeover":false,"remoteCaptions":false,"remoteAssistance":false},
            "iceServers":[],
            "relayOnly":false,
            "turnCredentialExpiresAt":null,
            "media":{"transport":"webrtc","gatewayRelaysMedia":false,"companionUsesBluetoothDongle":false,"relayOnly":false},
            "appId":"e2e19da6-dbb4-47e0-9250-7280f8f60ed2",
            "appSlug":"aokie-receptionist-78a80a"
        });
        let mut document = payload.as_object().unwrap().clone();
        document.insert("trustStatus".into(), json!("signed"));
        document.insert("signingKeyId".into(), json!("formlogic-ed25519-1"));
        document.insert("signatureAlgorithm".into(), json!("Ed25519"));
        document.insert("signature".into(), json!("signature"));
        document.insert(
            "signingKeyUrl".into(),
            json!("http://formlogic.local/api/public/signing-key"),
        );
        document.insert(
            "signatureEnvelope".into(),
            json!({"payload":payload,"signature":"signature","alg":"Ed25519","keyId":"formlogic-ed25519-1"}),
        );

        let (split, trust) = split_v2_document(Value::Object(document)).unwrap();
        let decoded: V2SignedPayload = serde_json::from_value(split.clone()).unwrap();
        let discovery_url =
            Url::parse("http://formlogic.local/api/app/aokie-receptionist-78a80a/aokie-discovery")
                .unwrap();
        assert!(validate_v2_payload(&decoded, &discovery_url, &trust.signing_key_url).is_ok());
        assert!(!decoded.relay_only);
        assert_eq!(decoded.turn_credential_expires_at.value(), None);
        assert!(!decoded.remote_consent.configured);

        let mut unknown_consent = split.clone();
        unknown_consent["remoteConsent"]["futureConsent"] = json!(false);
        assert!(serde_json::from_value::<V2SignedPayload>(unknown_consent).is_err());

        let mut unknown_media = split.clone();
        unknown_media["media"]["futureTransport"] = json!(false);
        assert!(serde_json::from_value::<V2SignedPayload>(unknown_media).is_err());

        let mut missing_required_nullable = split;
        missing_required_nullable
            .as_object_mut()
            .unwrap()
            .remove("turnCredentialExpiresAt");
        assert!(serde_json::from_value::<V2SignedPayload>(missing_required_nullable).is_err());
    }

    #[test]
    fn relay_only_turn_credentials_are_strict_and_map_to_runtime_ice() {
        let now = 1_800_000_000;
        let expiry = now + 300;
        let servers = vec![DiscoveryIceServer {
            urls: vec!["turns:turn.example:5349?transport=tcp".into()],
            username: "short-lived-user".into(),
            credential: "short-lived-secret".into(),
            expires_at: Some(expiry),
        }];
        let runtime =
            validate_managed_ice_configuration(&servers, true, Some(expiry), now).unwrap();
        assert_eq!(runtime.len(), 1);
        assert_eq!(runtime[0].username, "short-lived-user");
        assert!(validate_managed_ice_configuration(&servers, true, None, now).is_err());

        let mut stale = servers.clone();
        stale[0].expires_at = Some(now + MIN_TURN_CREDENTIAL_TTL_SECONDS);
        assert!(
            validate_managed_ice_configuration(&stale, true, stale[0].expires_at, now).is_err()
        );

        let stun_with_credentials = vec![DiscoveryIceServer {
            urls: vec!["stun:stun.example:3478".into()],
            username: "not-allowed".into(),
            credential: String::new(),
            expires_at: None,
        }];
        assert!(
            validate_managed_ice_configuration(&stun_with_credentials, false, None, now).is_err()
        );
    }

    #[cfg(feature = "managed-beta-local")]
    #[tokio::test]
    #[ignore = "requires the local FormLogic WAMP deployment"]
    async fn live_formlogic_signed_discovery_is_accepted_by_native_verification() {
        let discovery = fetch_discovery(
            "http://api.formlogic.local/api/app/aokie-receptionist-78a80a/aokie-discovery",
        )
        .await
        .unwrap();
        assert_eq!(discovery.schema_version, 2);
        assert_eq!(
            discovery.app_id.as_deref(),
            Some("e2e19da6-dbb4-47e0-9250-7280f8f60ed2")
        );
        assert!(discovery.signature_verified);
        assert!(!discovery.relay_only);
        assert_eq!(discovery.turn_credential_expires_at, None);
        assert!(discovery.remote_consent.is_some());
    }

    #[test]
    fn media_topology_rejects_claims_that_companion_uses_the_dongle() {
        let mut payload: V2SignedPayload = serde_json::from_value(json!({
            "schemaVersion":2,
            "issuer":"https://formlogic.example",
            "apiBaseUrl":"https://formlogic.example/api",
            "gatewayUrl":"wss://gateway.example/v2/realtime",
            "realtimeUrl":"wss://gateway.example/v2/realtime",
            "oauthAuthorizationUrl":"https://formlogic.example/oauth/authorize",
            "oauthTokenUrl":"https://formlogic.example/api/oauth/token",
            "oauthResource":"https://formlogic.example/api/aokie-companion",
            "admissionEndpoint":"https://formlogic.example/api/aokie-companion/admission",
            "clientId":"aokie-companion",
            "deploymentId":"deployment_a",
            "available":true,
            "scopesSupported":["aokie:state"],
            "features":["captions"],
            "remoteConsent":{"configured":true,"remoteMonitoring":false,"remoteConsult":false,"remoteTakeover":false,"remoteCaptions":true,"remoteAssistance":false},
            "iceServers":[],
            "relayOnly":false,
            "turnCredentialExpiresAt":null,
            "media":{"transport":"webrtc","gatewayRelaysMedia":false,"companionUsesBluetoothDongle":false,"relayOnly":false}
        })).unwrap();
        let discovery =
            Url::parse("https://formlogic.example/.well-known/aokie-companion").unwrap();
        assert!(validate_v2_payload(
            &payload,
            &discovery,
            "https://formlogic.example/api/public/signing-key"
        )
        .is_ok());
        payload.media.companion_uses_bluetooth_dongle = true;
        assert!(validate_v2_payload(
            &payload,
            &discovery,
            "https://formlogic.example/api/public/signing-key"
        )
        .is_err());
        payload.media.companion_uses_bluetooth_dongle = false;
        payload.media.relay_only = true;
        assert!(validate_v2_payload(
            &payload,
            &discovery,
            "https://formlogic.example/api/public/signing-key"
        )
        .is_err());
        payload.media.relay_only = false;
        payload.remote_consent.remote_captions = false;
        assert!(validate_v2_payload(
            &payload,
            &discovery,
            "https://formlogic.example/api/public/signing-key"
        )
        .is_err());
    }
}
