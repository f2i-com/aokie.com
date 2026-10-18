//! Native-only managed push endpoint lifecycle.
//!
//! Provider tokens are accepted only from native platform runtimes, sent with
//! the native OAuth bearer, and never returned through a Tauri command/event.

#![cfg_attr(not(any(target_os = "android", target_os = "ios")), allow(dead_code))]

use chrono::NaiveDateTime;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::AppHandle;

use crate::managed_auth::ManagedAuthState;

pub(crate) const ANDROID_FCM_TOKEN_KEY: &str = "aokie.fcm-registration-token.v1";
pub(crate) const ANDROID_FCM_REGISTRATION_KEY: &str = "aokie.fcm-registration-fingerprint.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativePushKind {
    Fcm,
    Apns,
    ApnsVoip,
}

impl NativePushKind {
    fn route_segment(self) -> &'static str {
        match self {
            Self::Fcm => "fcm",
            Self::Apns => "apns",
            Self::ApnsVoip => "apns_voip",
        }
    }

    fn provider(self) -> NativePushProvider {
        match self {
            Self::Fcm => NativePushProvider::Fcm,
            Self::Apns | Self::ApnsVoip => NativePushProvider::Apns,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativePushEnvironment {
    Sandbox,
    Production,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativePushMode {
    Managed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NativePushProvider {
    Fcm,
    Apns,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequiredNullable<T>(Option<T>);

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> Result<RequiredNullable<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(RequiredNullable)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedPushRegistrationRequest<'a> {
    mode: &'static str,
    environment: NativePushEnvironment,
    token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    topic: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedPushRegistrationResponse {
    endpoint: RedactedPushEndpoint,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RedactedPushEndpoint {
    id: String,
    app_id: String,
    device_id: String,
    kind: NativePushKind,
    mode: NativePushMode,
    provider: NativePushProvider,
    environment: NativePushEnvironment,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    topic: RequiredNullable<String>,
    fingerprint: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    invalidated_at: RequiredNullable<String>,
    rotated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeletePushResponse {
    success: bool,
}

pub(crate) async fn register_managed_push_endpoint(
    app: &AppHandle,
    auth: &ManagedAuthState,
    kind: NativePushKind,
    environment: NativePushEnvironment,
    token: &str,
    topic: Option<&str>,
) -> Result<String, String> {
    validate_provider_token(token)?;
    validate_topic(kind, topic)?;
    let fingerprint = token_fingerprint(token);
    let persisted = auth
        .active_binding(app)
        .await?
        .ok_or("managed sign-in is required for push registration")?;
    let route = endpoint_route(&persisted.1, kind)?;
    let body = serde_json::to_vec(&ManagedPushRegistrationRequest {
        mode: "managed",
        environment,
        token,
        topic,
    })
    .map_err(|_| "native push registration could not be encoded".to_string())?;
    let response = auth
        .mobile_api_request_expected(
            app,
            Method::PUT,
            &route,
            &[],
            Some(body),
            &[StatusCode::CREATED],
        )
        .await?;
    let parsed: ManagedPushRegistrationResponse = serde_json::from_slice(&response.body)
        .map_err(|_| "push registration response did not match its redacted schema".to_string())?;
    validate_endpoint(
        &parsed.endpoint,
        &response.app_id,
        kind,
        environment,
        topic,
        &fingerprint,
    )?;
    Ok(fingerprint)
}

pub(crate) async fn unregister_managed_push_endpoint(
    app: &AppHandle,
    auth: &ManagedAuthState,
    kind: NativePushKind,
) -> Result<(), String> {
    let Some((_, oauth_device_id)) = auth.active_binding(app).await? else {
        return Ok(());
    };
    let route = endpoint_route(&oauth_device_id, kind)?;
    let response = auth
        .mobile_api_request_expected(
            app,
            Method::DELETE,
            &route,
            &[],
            None,
            &[StatusCode::OK, StatusCode::NOT_FOUND],
        )
        .await?;
    if response.status == StatusCode::NOT_FOUND {
        return Ok(());
    }
    let parsed: DeletePushResponse = serde_json::from_slice(&response.body)
        .map_err(|_| "push unregister response did not match its strict schema".to_string())?;
    if !parsed.success {
        return Err("push endpoint was not invalidated".into());
    }
    Ok(())
}

#[cfg(target_os = "android")]
pub(crate) async fn reconcile_android_fcm(
    app: &AppHandle,
    auth: &ManagedAuthState,
) -> Result<(), String> {
    let diagnostics = crate::android_runtime::diagnostics(app).await?;
    if !diagnostics.fcm_configured {
        return Err("FCM configuration is required before endpoint registration".into());
    }
    let token = crate::android_runtime::secure_store_get(app, ANDROID_FCM_TOKEN_KEY)
        .await?
        .ok_or("FCM registration token is pending")?;
    validate_provider_token(&token)?;
    let fingerprint = token_fingerprint(&token);
    let binding = auth
        .active_registration_binding(app)
        .await?
        .ok_or("managed sign-in is required for push registration")?;
    let marker = registration_marker(&fingerprint, &binding);
    // The marker only saves a repeat registration. An unreadable one (the
    // store now reports read errors instead of "absent") is treated as
    // missing: registering again is harmless and rewrites it, while
    // failing here would never register this phone at all.
    let recorded = crate::android_runtime::secure_store_get(app, ANDROID_FCM_REGISTRATION_KEY)
        .await
        .unwrap_or(None);
    if recorded.as_deref() == Some(marker.as_str()) {
        return Ok(());
    }
    let registered = register_managed_push_endpoint(
        app,
        auth,
        NativePushKind::Fcm,
        NativePushEnvironment::Production,
        &token,
        None,
    )
    .await?;
    if registered != fingerprint {
        return Err("push registration answered for a different provider token".into());
    }
    // The marker is only a SHA-256 digest. The token remains in Android
    // Keystore and never crosses a renderer command or event.
    crate::android_runtime::secure_store_put(app, ANDROID_FCM_REGISTRATION_KEY, &marker).await
}

/// What the registration marker records: this provider token registered
/// with THIS server, deployment, profile, app and OAuth device. A marker of
/// the token alone made a sign-in to another server, or a device the server
/// re-created, look already registered, so background calls never alerted.
/// Markers written by earlier builds (the bare fingerprint) never match, so
/// each phone registers once more after the update.
fn registration_marker(token_fingerprint: &str, binding: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"aokie/push-registration-marker/v2\0");
    digest.update(token_fingerprint.as_bytes());
    digest.update(b"\0");
    digest.update(binding.as_bytes());
    format!("v2:{:x}", digest.finalize())
}

fn endpoint_route(oauth_device_id: &str, kind: NativePushKind) -> Result<String, String> {
    validate_safe_id(oauth_device_id, "push OAuth device id")?;
    Ok(format!(
        "/api/aokie-companion/mobile/devices/{oauth_device_id}/push-endpoints/{}",
        kind.route_segment()
    ))
}

fn validate_provider_token(token: &str) -> Result<(), String> {
    if !(16..=4_096).contains(&token.len()) || token.chars().any(char::is_control) {
        Err("native push provider token is invalid".into())
    } else {
        Ok(())
    }
}

fn validate_topic(kind: NativePushKind, topic: Option<&str>) -> Result<(), String> {
    let valid = topic.is_none_or(|topic| {
        !topic.is_empty()
            && topic.len() <= 255
            && topic
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    });
    if !valid || (!matches!(kind, NativePushKind::Fcm) && topic.is_none()) {
        Err("native push topic is invalid".into())
    } else {
        Ok(())
    }
}

fn validate_endpoint(
    endpoint: &RedactedPushEndpoint,
    expected_app_id: &str,
    expected_kind: NativePushKind,
    expected_environment: NativePushEnvironment,
    expected_topic: Option<&str>,
    expected_fingerprint: &str,
) -> Result<(), String> {
    for (value, label) in [
        (&endpoint.id, "push endpoint id"),
        (&endpoint.app_id, "push endpoint app id"),
        (&endpoint.device_id, "push endpoint device id"),
    ] {
        validate_safe_id(value, label)?;
    }
    if endpoint.app_id != expected_app_id
        || endpoint.kind != expected_kind
        || endpoint.mode != NativePushMode::Managed
        || endpoint.provider != expected_kind.provider()
        || endpoint.environment != expected_environment
        || endpoint.topic.0.as_deref() != expected_topic
        || endpoint.fingerprint != expected_fingerprint
        || endpoint.invalidated_at.0.is_some()
        || !valid_db_timestamp(&endpoint.rotated_at)
    {
        return Err("push endpoint response is not bound to the native registration".into());
    }
    Ok(())
}

fn validate_safe_id(value: &str, label: &str) -> Result<(), String> {
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

fn token_fingerprint(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn valid_db_timestamp(value: &str) -> bool {
    value.len() == 19 && NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_tokens_never_enter_redacted_registration_responses() {
        let token = "provider-token-that-stays-native";
        let fingerprint = token_fingerprint(token);
        let encoded = serde_json::to_value(ManagedPushRegistrationRequest {
            mode: "managed",
            environment: NativePushEnvironment::Production,
            token,
            topic: None,
        })
        .unwrap();
        assert_eq!(encoded["token"], token);
        assert_eq!(fingerprint.len(), 64);
        assert!(!fingerprint.contains(token));
    }

    #[test]
    fn the_registration_marker_changes_with_the_server_or_device_not_only_the_token() {
        let fingerprint = token_fingerprint("provider-token-that-stays-native");
        let server_a = "https://a.example/.well-known/aokie-companion\ndeployment_1\nprofile_a\napp_1\noauth_device_1";
        let server_b = "https://b.example/.well-known/aokie-companion\ndeployment_2\nprofile_b\napp_1\noauth_device_1";
        let recreated = "https://a.example/.well-known/aokie-companion\ndeployment_1\nprofile_a\napp_1\noauth_device_2";
        let marker = registration_marker(&fingerprint, server_a);
        assert_eq!(marker, registration_marker(&fingerprint, server_a));
        assert_ne!(marker, registration_marker(&fingerprint, server_b));
        assert_ne!(marker, registration_marker(&fingerprint, recreated));
        assert_ne!(
            marker,
            registration_marker(&token_fingerprint("a-rotated-provider-token"), server_a)
        );
        // An earlier build's marker (the bare fingerprint) never matches, and
        // the marker reveals neither the token nor the binding.
        assert_ne!(marker, fingerprint);
        assert!(!marker.contains("oauth_device_1") && !marker.contains(&fingerprint));
    }

    #[test]
    fn push_routes_are_exact_and_device_bound() {
        assert_eq!(
            endpoint_route("oauth_device_1", NativePushKind::ApnsVoip).unwrap(),
            "/api/aokie-companion/mobile/devices/oauth_device_1/push-endpoints/apns_voip"
        );
        assert!(endpoint_route("../escape", NativePushKind::Fcm).is_err());
    }

    #[test]
    fn apns_requires_a_safe_topic_while_fcm_may_omit_it() {
        assert!(validate_topic(NativePushKind::Fcm, None).is_ok());
        assert!(validate_topic(NativePushKind::Apns, None).is_err());
        assert!(validate_topic(NativePushKind::ApnsVoip, Some("com.aokie.companion.voip")).is_ok());
        assert!(validate_topic(NativePushKind::Apns, Some("bad/topic")).is_err());
    }

    #[test]
    fn redacted_endpoint_is_bound_to_token_fingerprint_and_provider() {
        let token = "provider-token-that-stays-native";
        let endpoint = RedactedPushEndpoint {
            id: "endpoint_1".into(),
            app_id: "app_1".into(),
            device_id: "device_record_1".into(),
            kind: NativePushKind::Fcm,
            mode: NativePushMode::Managed,
            provider: NativePushProvider::Fcm,
            environment: NativePushEnvironment::Production,
            topic: RequiredNullable(None),
            fingerprint: token_fingerprint(token),
            invalidated_at: RequiredNullable(None),
            rotated_at: "2026-07-16 03:00:00".into(),
        };
        assert!(validate_endpoint(
            &endpoint,
            "app_1",
            NativePushKind::Fcm,
            NativePushEnvironment::Production,
            None,
            &token_fingerprint(token),
        )
        .is_ok());
        assert!(validate_endpoint(
            &endpoint,
            "app_1",
            NativePushKind::Fcm,
            NativePushEnvironment::Production,
            None,
            &"0".repeat(64),
        )
        .is_err());
    }
}
