//! One-use, owner-confirmed pairing with an Aokie Desktop endpoint.
//!
//! The Desktop offer is deliberately unsigned public binding data. Native
//! code validates its shape and lifetime, then the owner must compare the full
//! Desktop thumbprint out of band. Only after that explicit decision does the
//! installation identity sign a response. The private seed never crosses IPC.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use aokie_protocol::v2::EndpointPublicKey;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, State};
use tokio::sync::Mutex;

use crate::endpoint_identity::{load_or_create, EndpointIdentity};
use crate::server_profiles::{load_active_pairing_profile, PairingProfileBinding};

const MAX_OFFER_BYTES: usize = 16 * 1024;
const MAX_ID_BYTES: usize = 200;
const MAX_PENDING_REVIEWS: usize = 16;
const PAIRING_TTL_SECONDS: u64 = 10 * 60;
const RESPONSE_TTL_SECONDS: u64 = 2 * 60;
const CLOCK_SKEW_SECONDS: u64 = 5;
const PAIRING_RESPONSE_DOMAIN: &str = "aokie/v2/mobile-pairing-response";

#[derive(Clone, Default)]
pub struct DesktopPairingState {
    pending: Arc<Mutex<HashMap<String, PendingDesktopPairing>>>,
}

#[derive(Clone)]
struct PendingDesktopPairing {
    review_id: String,
    profile_id: String,
    payload: PairingPayload,
    device_id: String,
    mobile_endpoint_key: EndpointPublicKey,
    desktop_fingerprint: String,
    mobile_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairingPayload {
    kind: String,
    schema_version: u16,
    app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    desktop_connection_id: String,
    desktop_endpoint_key: EndpointPublicKey,
    nonce: String,
    jti: String,
    issued_at: u64,
    expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MobilePairingClaims {
    app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    desktop_connection_id: String,
    desktop_key_thumbprint: String,
    device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    mobile_endpoint_key: EndpointPublicKey,
    pairing_nonce: String,
    jti: String,
    issued_at: u64,
    expires_at: u64,
}

impl MobilePairingClaims {
    fn signing_bytes(&self) -> Result<Vec<u8>, String> {
        domain_separated_canonical(PAIRING_RESPONSE_DOMAIN, self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MobilePairingResponse {
    kind: String,
    schema_version: u16,
    claims: MobilePairingClaims,
    signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewDesktopPairingOfferRequest {
    profile_id: String,
    offer_json: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopPairingReview {
    review_id: String,
    profile_id: String,
    app_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    desktop_connection_id: String,
    device_id: String,
    desktop_key_thumbprint: String,
    desktop_fingerprint: String,
    mobile_key_thumbprint: String,
    mobile_fingerprint: String,
    issued_at: u64,
    expires_at: u64,
    unsigned_public_offer: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmDesktopPairingRequest {
    review_id: String,
    profile_id: String,
    desktop_key_thumbprint: String,
    desktop_fingerprint: String,
    mobile_key_thumbprint: String,
    mobile_fingerprint: String,
    display_name: Option<String>,
    desktop_thumbprint_confirmed: bool,
    mobile_fingerprint_acknowledged: bool,
    approved: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopPairingDecision {
    approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_json: Option<String>,
    desktop_key_thumbprint: String,
    mobile_key_thumbprint: String,
    mobile_fingerprint: String,
    device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedDesktopPairing {
    schema_version: u8,
    profile_id: String,
    app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    desktop_connection_id: String,
    desktop_endpoint_key: EndpointPublicKey,
    mobile_key_thumbprint: String,
    device_id: String,
    confirmed_at: u64,
}

#[tauri::command]
pub async fn native_review_desktop_pairing_offer(
    app: AppHandle,
    state: State<'_, DesktopPairingState>,
    request: ReviewDesktopPairingOfferRequest,
) -> Result<DesktopPairingReview, String> {
    let identity = load_or_create(&app).await?;
    let profile = load_active_pairing_profile(&app, &request.profile_id).await?;
    if profile.endpoint_key_thumbprint != identity.thumbprint() {
        return Err(
            "active server profile is bound to a different Companion installation identity".into(),
        );
    }
    let now = unix_now()?;
    let review_id = random_id("pairing_review");
    let (pending, review) =
        prepare_review_at(&request.offer_json, &identity, &profile, review_id, now)?;
    let mut reviews = state.pending.lock().await;
    reviews.retain(|_, candidate| candidate.payload.expires_at > now);
    // Re-importing the same offer replaces its prior local review. Desktop's
    // in-memory challenge remains the one-use source of truth.
    reviews.retain(|_, candidate| {
        candidate.payload.nonce != pending.payload.nonce
            || candidate.payload.jti != pending.payload.jti
    });
    if reviews.len() >= MAX_PENDING_REVIEWS {
        return Err("too many Desktop pairing reviews are pending".into());
    }
    reviews.insert(pending.review_id.clone(), pending);
    Ok(review)
}

#[tauri::command]
pub async fn native_confirm_desktop_pairing(
    app: AppHandle,
    state: State<'_, DesktopPairingState>,
    request: ConfirmDesktopPairingRequest,
) -> Result<DesktopPairingDecision, String> {
    let pending = state
        .pending
        .lock()
        .await
        .remove(&request.review_id)
        .ok_or("Desktop pairing review is no longer pending")?;
    let now = unix_now()?;
    validate_confirmation(&pending, &request, now)?;
    if !request.approved {
        return Ok(DesktopPairingDecision {
            approved: false,
            response_json: None,
            desktop_key_thumbprint: pending.payload.desktop_endpoint_key.thumbprint,
            mobile_key_thumbprint: pending.mobile_endpoint_key.thumbprint,
            mobile_fingerprint: pending.mobile_fingerprint,
            device_id: pending.device_id,
            expires_at: None,
        });
    }

    let display_name = request
        .display_name
        .as_deref()
        .unwrap_or("Aokie Companion")
        .trim();
    validate_display_name(display_name)?;
    let identity = load_or_create(&app).await?;
    if identity.public_key() != &pending.mobile_endpoint_key {
        return Err("Companion installation identity changed during pairing".into());
    }
    let response = create_response_at(&pending, &identity, display_name, now)?;
    persist_confirmed_pairing(&app, &pending, now).await?;
    crate::peer_trust::persist_confirmed_peer_pin(
        &app,
        &pending.profile_id,
        &pending.payload.app_id,
        &pending.device_id,
        &pending.payload.desktop_endpoint_key.thumbprint,
        now,
    )
    .await?;
    let response_json = serde_json::to_string_pretty(&response)
        .map_err(|_| "could not encode the Desktop pairing response".to_string())?;
    Ok(DesktopPairingDecision {
        approved: true,
        response_json: Some(response_json),
        desktop_key_thumbprint: pending.payload.desktop_endpoint_key.thumbprint,
        mobile_key_thumbprint: pending.mobile_endpoint_key.thumbprint,
        mobile_fingerprint: pending.mobile_fingerprint,
        device_id: pending.device_id,
        expires_at: Some(response.claims.expires_at),
    })
}

fn prepare_review_at(
    offer_json: &str,
    identity: &EndpointIdentity,
    profile: &PairingProfileBinding,
    review_id: String,
    now: u64,
) -> Result<(PendingDesktopPairing, DesktopPairingReview), String> {
    safe_id("review id", &review_id)?;
    validate_profile_binding(profile, identity)?;
    let payload = parse_offer_at(offer_json, now)?;
    if payload.app_id != profile.app_id {
        return Err("Desktop pairing offer belongs to a different server-profile app".into());
    }
    if payload.desktop_endpoint_key.thumbprint == identity.thumbprint() {
        return Err("Desktop and Companion cannot use the same endpoint identity".into());
    }
    let device_id = profile.device_id.clone();
    let desktop_fingerprint = display_fingerprint(&payload.desktop_endpoint_key)?;
    let mobile_fingerprint = display_fingerprint(identity.public_key())?;
    let pending = PendingDesktopPairing {
        review_id: review_id.clone(),
        profile_id: profile.profile_id.clone(),
        payload: payload.clone(),
        device_id: device_id.clone(),
        mobile_endpoint_key: identity.public_key().clone(),
        desktop_fingerprint: desktop_fingerprint.clone(),
        mobile_fingerprint: mobile_fingerprint.clone(),
    };
    let review = DesktopPairingReview {
        review_id,
        profile_id: profile.profile_id.clone(),
        app_id: payload.app_id,
        workspace_id: payload.workspace_id,
        desktop_connection_id: payload.desktop_connection_id,
        device_id,
        desktop_key_thumbprint: payload.desktop_endpoint_key.thumbprint,
        desktop_fingerprint,
        mobile_key_thumbprint: identity.thumbprint().to_owned(),
        mobile_fingerprint,
        issued_at: payload.issued_at,
        expires_at: payload.expires_at,
        unsigned_public_offer: true,
    };
    Ok((pending, review))
}

fn parse_offer_at(offer_json: &str, now: u64) -> Result<PairingPayload, String> {
    let trimmed = offer_json.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_OFFER_BYTES {
        return Err("Desktop pairing offer must be 1-16384 UTF-8 bytes".into());
    }
    let payload: PairingPayload = serde_json::from_str(trimmed).map_err(|_| {
        "Desktop pairing offer is malformed or contains unsupported fields".to_string()
    })?;
    if payload.kind != "aokie_mobile_pairing" || payload.schema_version != 2 {
        return Err("unsupported Desktop pairing offer".into());
    }
    for (value, label) in [
        (&payload.app_id, "app id"),
        (&payload.desktop_connection_id, "Desktop connection id"),
        (&payload.nonce, "pairing nonce"),
        (&payload.jti, "pairing jti"),
    ] {
        safe_id(label, value)?;
    }
    if let Some(workspace_id) = payload.workspace_id.as_deref() {
        safe_id("workspace id", workspace_id)?;
    }
    payload
        .desktop_endpoint_key
        .validate()
        .map_err(|_| "Desktop endpoint public key is invalid".to_string())?;
    if payload.issued_at > now.saturating_add(CLOCK_SKEW_SECONDS)
        || payload.expires_at <= now
        || payload.expires_at <= payload.issued_at
        || payload.expires_at.saturating_sub(payload.issued_at) > PAIRING_TTL_SECONDS
    {
        return Err("Desktop pairing offer is expired or has an invalid lifetime".into());
    }
    Ok(payload)
}

fn validate_confirmation(
    pending: &PendingDesktopPairing,
    request: &ConfirmDesktopPairingRequest,
    now: u64,
) -> Result<(), String> {
    safe_id("review id", &request.review_id)?;
    safe_id("profile id", &request.profile_id)?;
    for (value, label) in [
        (&request.desktop_key_thumbprint, "Desktop key thumbprint"),
        (&request.desktop_fingerprint, "Desktop fingerprint"),
        (&request.mobile_key_thumbprint, "mobile key thumbprint"),
        (&request.mobile_fingerprint, "mobile fingerprint"),
    ] {
        safe_id(label, value)?;
    }
    if pending.review_id != request.review_id
        || pending.profile_id != request.profile_id
        || pending.payload.desktop_endpoint_key.thumbprint != request.desktop_key_thumbprint
        || pending.desktop_fingerprint != request.desktop_fingerprint
        || pending.mobile_endpoint_key.thumbprint != request.mobile_key_thumbprint
        || pending.mobile_fingerprint != request.mobile_fingerprint
        || pending.payload.expires_at <= now
    {
        return Err(
            "Desktop pairing confirmation is stale or does not match the reviewed fingerprints"
                .into(),
        );
    }
    if request.approved
        && (!request.desktop_thumbprint_confirmed || !request.mobile_fingerprint_acknowledged)
    {
        return Err("both displayed pairing fingerprints require explicit confirmation".into());
    }
    if !request.approved
        && (request.desktop_thumbprint_confirmed || request.mobile_fingerprint_acknowledged)
    {
        return Err("rejected Desktop pairing cannot contain fingerprint approvals".into());
    }
    Ok(())
}

fn create_response_at(
    pending: &PendingDesktopPairing,
    identity: &EndpointIdentity,
    display_name: &str,
    now: u64,
) -> Result<MobilePairingResponse, String> {
    if pending.payload.expires_at <= now {
        return Err("Desktop pairing offer expired before confirmation".into());
    }
    let expires_at = pending
        .payload
        .expires_at
        .min(now.saturating_add(RESPONSE_TTL_SECONDS));
    if expires_at <= now {
        return Err("Desktop pairing response has no valid lifetime".into());
    }
    let claims = MobilePairingClaims {
        app_id: pending.payload.app_id.clone(),
        workspace_id: pending.payload.workspace_id.clone(),
        desktop_connection_id: pending.payload.desktop_connection_id.clone(),
        desktop_key_thumbprint: pending.payload.desktop_endpoint_key.thumbprint.clone(),
        device_id: pending.device_id.clone(),
        display_name: Some(display_name.to_owned()),
        mobile_endpoint_key: pending.mobile_endpoint_key.clone(),
        pairing_nonce: pending.payload.nonce.clone(),
        jti: pending.payload.jti.clone(),
        issued_at: now,
        expires_at,
    };
    let signature = identity.sign_desktop_pairing_response(&claims.signing_bytes()?);
    claims
        .mobile_endpoint_key
        .verify(&claims.signing_bytes()?, &signature)
        .map_err(|_| "generated mobile pairing signature did not verify".to_string())?;
    Ok(MobilePairingResponse {
        kind: "aokie_mobile_pairing_response".into(),
        schema_version: 2,
        claims,
        signature,
    })
}

async fn persist_confirmed_pairing(
    app: &AppHandle,
    pending: &PendingDesktopPairing,
    now: u64,
) -> Result<(), String> {
    let binding = PersistedDesktopPairing {
        schema_version: 1,
        profile_id: pending.profile_id.clone(),
        app_id: pending.payload.app_id.clone(),
        workspace_id: pending.payload.workspace_id.clone(),
        desktop_connection_id: pending.payload.desktop_connection_id.clone(),
        desktop_endpoint_key: pending.payload.desktop_endpoint_key.clone(),
        mobile_key_thumbprint: pending.mobile_endpoint_key.thumbprint.clone(),
        device_id: pending.device_id.clone(),
        confirmed_at: now,
    };
    validate_persisted_pairing(&binding)?;
    let account = pairing_account(&binding);
    let encoded = serde_json::to_string(&binding)
        .map_err(|_| "could not encode confirmed Desktop pairing".to_string())?;
    crate::endpoint_identity::native_store_put(app, &account, &encoded).await?;
    let stored = crate::endpoint_identity::native_store_get(app, &account)
        .await?
        .ok_or("confirmed Desktop pairing did not persist")?;
    if stored.len() > 8_192 {
        return Err("saved Desktop pairing is too large".into());
    }
    let stored: PersistedDesktopPairing = serde_json::from_str(&stored)
        .map_err(|_| "saved Desktop pairing is malformed".to_string())?;
    validate_persisted_pairing(&stored)?;
    if stored != binding {
        return Err("Desktop pairing persistence verification failed".into());
    }
    Ok(())
}

fn validate_persisted_pairing(binding: &PersistedDesktopPairing) -> Result<(), String> {
    if binding.schema_version != 1 || binding.confirmed_at == 0 {
        return Err("saved Desktop pairing has an unsupported version".into());
    }
    for (value, label) in [
        (&binding.profile_id, "profile id"),
        (&binding.app_id, "app id"),
        (&binding.desktop_connection_id, "Desktop connection id"),
        (&binding.mobile_key_thumbprint, "mobile key thumbprint"),
        (&binding.device_id, "mobile device id"),
    ] {
        safe_id(label, value)?;
    }
    if let Some(workspace_id) = binding.workspace_id.as_deref() {
        safe_id("workspace id", workspace_id)?;
    }
    binding
        .desktop_endpoint_key
        .validate()
        .map_err(|_| "saved Desktop endpoint key is invalid".to_string())
}

fn pairing_account(binding: &PersistedDesktopPairing) -> String {
    let digest = Sha256::digest(binding.profile_id.as_bytes());
    format!("aokie-desktop-pairing-v1:{digest:x}")
}

fn validate_profile_binding(
    profile: &PairingProfileBinding,
    identity: &EndpointIdentity,
) -> Result<(), String> {
    for (value, label) in [
        (&profile.profile_id, "profile id"),
        (&profile.app_id, "profile app id"),
        (&profile.device_id, "profile device id"),
        (
            &profile.endpoint_key_thumbprint,
            "profile endpoint key thumbprint",
        ),
    ] {
        safe_id(label, value)?;
    }
    if profile.endpoint_key_thumbprint != identity.thumbprint() {
        return Err(
            "server profile is bound to a different Companion installation identity".into(),
        );
    }
    Ok(())
}

fn display_fingerprint(key: &EndpointPublicKey) -> Result<String, String> {
    key.validate()
        .map_err(|_| "endpoint public key is invalid".to_string())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(&key.public_key)
        .map_err(|_| "endpoint public key is malformed".to_string())?;
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(79);
    for (index, byte) in digest.iter().enumerate() {
        if index > 0 && index % 2 == 0 {
            encoded.push(':');
        }
        write!(encoded, "{byte:02X}").expect("writing to a String cannot fail");
    }
    Ok(encoded)
}

fn validate_display_name(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 120 || value.chars().any(char::is_control) {
        Err("Companion display name must be 1-120 printable characters".into())
    } else {
        Ok(())
    }
}

fn safe_id(field: &str, value: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(format!("{field} is not a safe protocol identifier"))
    }
}

fn random_id(prefix: &str) -> String {
    let mut value = [0_u8; 16];
    OsRng.fill_bytes(&mut value);
    format!("{prefix}_{}", URL_SAFE_NO_PAD.encode(value))
}

fn unix_now() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before Unix epoch".into())
}

fn domain_separated_canonical<T: Serialize>(domain: &str, value: &T) -> Result<Vec<u8>, String> {
    let value = serde_json::to_value(value)
        .map_err(|_| "could not serialize mobile pairing claims".to_string())?;
    let mut bytes = domain.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&canonical_json_value(&value)?);
    Ok(bytes)
}

fn canonical_json_value(value: &Value) -> Result<Vec<u8>, String> {
    fn write_value(value: &Value, output: &mut Vec<u8>) -> Result<(), String> {
        match value {
            Value::Null => output.extend_from_slice(b"null"),
            Value::Bool(true) => output.extend_from_slice(b"true"),
            Value::Bool(false) => output.extend_from_slice(b"false"),
            Value::Number(number) => {
                if !number.is_u64() && !number.is_i64() {
                    return Err(
                        "signed pairing claims cannot contain floating-point numbers".into(),
                    );
                }
                output.extend_from_slice(number.to_string().as_bytes());
            }
            Value::String(text) => output.extend_from_slice(
                serde_json::to_string(text)
                    .map_err(|_| "could not canonicalize pairing text".to_string())?
                    .as_bytes(),
            ),
            Value::Array(values) => {
                output.push(b'[');
                for (index, item) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    write_value(item, output)?;
                }
                output.push(b']');
            }
            Value::Object(map) => {
                output.push(b'{');
                let mut keys = map.keys().collect::<Vec<_>>();
                keys.sort();
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    write_value(&Value::String(key.clone()), output)?;
                    output.push(b':');
                    write_value(&map[key], output)?;
                }
                output.push(b'}');
            }
        }
        Ok(())
    }

    let mut output = Vec::new();
    write_value(value, &mut output)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn offer_json(now: u64) -> String {
        let desktop = SigningKey::from_bytes(&[9; 32]);
        serde_json::to_string(&PairingPayload {
            kind: "aokie_mobile_pairing".into(),
            schema_version: 2,
            app_id: "app_a".into(),
            workspace_id: Some("workspace_a".into()),
            desktop_connection_id: "desktop_connection_a".into(),
            desktop_endpoint_key: EndpointPublicKey::from_ed25519_bytes(
                &desktop.verifying_key().to_bytes(),
            ),
            nonce: "pairing_nonce_a".into(),
            jti: "pairing_jti_a".into(),
            issued_at: now,
            expires_at: now + 300,
        })
        .unwrap()
    }

    fn profile(identity: &EndpointIdentity) -> PairingProfileBinding {
        PairingProfileBinding {
            profile_id: "profile_a".into(),
            app_id: "app_a".into(),
            device_id: "mobile_profile_device_a".into(),
            endpoint_key_thumbprint: identity.thumbprint().into(),
        }
    }

    #[test]
    fn reviewed_offer_requires_exact_values_and_yields_desktop_compatible_proof() {
        let now = 1_700_000_000;
        let identity = EndpointIdentity::from_secret([7; 32]).unwrap();
        let profile = profile(&identity);
        let (pending, review) = prepare_review_at(
            &offer_json(now),
            &identity,
            &profile,
            "pairing_review_a".into(),
            now + 1,
        )
        .unwrap();
        assert!(review.unsigned_public_offer);
        assert_eq!(
            review.desktop_key_thumbprint,
            pending.payload.desktop_endpoint_key.thumbprint
        );
        assert_eq!(
            review.mobile_fingerprint,
            display_fingerprint(identity.public_key()).unwrap()
        );

        let mut request = ConfirmDesktopPairingRequest {
            review_id: review.review_id,
            profile_id: review.profile_id,
            desktop_key_thumbprint: review.desktop_key_thumbprint,
            desktop_fingerprint: review.desktop_fingerprint,
            mobile_key_thumbprint: review.mobile_key_thumbprint,
            mobile_fingerprint: review.mobile_fingerprint,
            display_name: Some("Reception desk".into()),
            desktop_thumbprint_confirmed: true,
            mobile_fingerprint_acknowledged: true,
            approved: true,
        };
        validate_confirmation(&pending, &request, now + 2).unwrap();
        request.mobile_fingerprint_acknowledged = false;
        assert!(validate_confirmation(&pending, &request, now + 2).is_err());
        request.mobile_fingerprint_acknowledged = true;
        let response = create_response_at(&pending, &identity, "Reception desk", now + 2).unwrap();
        response
            .claims
            .mobile_endpoint_key
            .verify(
                &response.claims.signing_bytes().unwrap(),
                &response.signature,
            )
            .unwrap();
        assert_eq!(response.kind, "aokie_mobile_pairing_response");
        assert_eq!(response.claims.jti, "pairing_jti_a");
        assert_eq!(response.claims.pairing_nonce, "pairing_nonce_a");
        assert_eq!(response.claims.device_id, profile.device_id);
        assert_eq!(
            response.claims.desktop_key_thumbprint,
            pending.payload.desktop_endpoint_key.thumbprint
        );
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("seed"));
    }

    #[test]
    fn offer_parser_rejects_unknown_fields_bad_keys_and_expiry() {
        let now = 1_700_000_000;
        let mut unknown: Value = serde_json::from_str(&offer_json(now)).unwrap();
        unknown["signature"] = Value::String("this-offer-is-not-signed".into());
        assert!(parse_offer_at(&unknown.to_string(), now).is_err());

        let mut wrong_thumbprint: Value = serde_json::from_str(&offer_json(now)).unwrap();
        wrong_thumbprint["desktopEndpointKey"]["thumbprint"] = Value::String("wrong".into());
        assert!(parse_offer_at(&wrong_thumbprint.to_string(), now).is_err());

        let mut expired: Value = serde_json::from_str(&offer_json(now)).unwrap();
        expired["expiresAt"] = Value::from(now);
        assert!(parse_offer_at(&expired.to_string(), now).is_err());
    }

    #[test]
    fn confirmation_mismatch_consumes_no_signing_authority() {
        let now = 1_700_000_000;
        let identity = EndpointIdentity::from_secret([7; 32]).unwrap();
        let profile = profile(&identity);
        let (pending, review) = prepare_review_at(
            &offer_json(now),
            &identity,
            &profile,
            "pairing_review_a".into(),
            now,
        )
        .unwrap();
        let request = ConfirmDesktopPairingRequest {
            review_id: review.review_id,
            profile_id: review.profile_id,
            desktop_key_thumbprint: "substituted_desktop_key".into(),
            desktop_fingerprint: review.desktop_fingerprint,
            mobile_key_thumbprint: review.mobile_key_thumbprint,
            mobile_fingerprint: review.mobile_fingerprint,
            display_name: Some("Reception desk".into()),
            desktop_thumbprint_confirmed: true,
            mobile_fingerprint_acknowledged: true,
            approved: true,
        };
        assert!(validate_confirmation(&pending, &request, now + 1).is_err());
    }

    #[test]
    fn profile_app_device_and_installation_bindings_cannot_cross() {
        let now = 1_700_000_000;
        let identity = EndpointIdentity::from_secret([7; 32]).unwrap();
        let profile = profile(&identity);

        let mut wrong_app: Value = serde_json::from_str(&offer_json(now)).unwrap();
        wrong_app["appId"] = Value::String("app_b".into());
        assert!(prepare_review_at(
            &wrong_app.to_string(),
            &identity,
            &profile,
            "pairing_review_wrong_app".into(),
            now,
        )
        .is_err());

        let mut wrong_installation = profile.clone();
        wrong_installation.endpoint_key_thumbprint = EndpointIdentity::from_secret([8; 32])
            .unwrap()
            .thumbprint()
            .into();
        assert!(prepare_review_at(
            &offer_json(now),
            &identity,
            &wrong_installation,
            "pairing_review_wrong_device".into(),
            now,
        )
        .is_err());

        let (pending, review) = prepare_review_at(
            &offer_json(now),
            &identity,
            &profile,
            "pairing_review_profile_a".into(),
            now,
        )
        .unwrap();
        let cross_profile = ConfirmDesktopPairingRequest {
            review_id: review.review_id,
            profile_id: "profile_b".into(),
            desktop_key_thumbprint: review.desktop_key_thumbprint,
            desktop_fingerprint: review.desktop_fingerprint,
            mobile_key_thumbprint: review.mobile_key_thumbprint,
            mobile_fingerprint: review.mobile_fingerprint,
            display_name: Some("Reception desk".into()),
            desktop_thumbprint_confirmed: true,
            mobile_fingerprint_acknowledged: true,
            approved: true,
        };
        assert!(validate_confirmation(&pending, &cross_profile, now + 1).is_err());
    }

    #[test]
    fn persisted_account_hides_customer_binding_text() {
        let desktop = SigningKey::from_bytes(&[9; 32]);
        let binding = PersistedDesktopPairing {
            schema_version: 1,
            profile_id: "profile_private_customer".into(),
            app_id: "private_customer".into(),
            workspace_id: Some("private_workspace".into()),
            desktop_connection_id: "private_desktop".into(),
            desktop_endpoint_key: EndpointPublicKey::from_ed25519_bytes(
                &desktop.verifying_key().to_bytes(),
            ),
            mobile_key_thumbprint: EndpointIdentity::from_secret([7; 32])
                .unwrap()
                .thumbprint()
                .into(),
            device_id: "companion_device".into(),
            confirmed_at: 1,
        };
        let account = pairing_account(&binding);
        assert!(account.starts_with("aokie-desktop-pairing-v1:"));
        assert!(!account.contains("private"));
    }
}
