mod android_runtime;
mod companion_relay;
mod desktop_pairing;
mod discovery;
mod endpoint_identity;
mod managed_auth;
mod media;
mod mobile_api;
mod peer_trust;
mod push_registration;
mod realtime;
mod realtime_v2;
mod server_profiles;

use desktop_pairing::{
    native_confirm_desktop_pairing, native_review_desktop_pairing_offer, DesktopPairingState,
};
use discovery::{discover_deployment, DiscoveryDocument};
use managed_auth::{managed_authorize, managed_forget, managed_restore, ManagedAuthState};
use media::{
    media_accept_answer, media_add_ice_candidate, media_arm_microphone, media_close,
    media_create_offer, media_disarm_microphone, media_get_audio_devices, media_renew_lease,
    media_revoke, media_select_audio_devices, NativeMediaState,
};
use mobile_api::{
    companion_availability, companion_bootstrap, companion_call_record_detail,
    companion_call_records, companion_history, companion_routing, companion_set_availability,
};
use peer_trust::{native_confirm_desktop_peer_trust, PeerTrustState};
use realtime::{
    realtime_connect, realtime_disconnect, realtime_send, RealtimeConfig, RealtimeState,
};
use realtime_v2::{
    realtime_v2_answer_assistance, realtime_v2_confirm_end_caller, realtime_v2_prepare_end_caller,
    realtime_v2_request_lease, realtime_v2_revoke_lease,
};
use serde::Serialize;
use server_profiles::{
    native_begin_custom_server_authorization, native_confirm_custom_server_trust,
    native_connect_profile, native_forget_server_profile, native_list_server_profiles,
    native_rotate_server_trust, ServerProfileState,
};
use tauri::{AppHandle, State};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeCapabilities {
    platform: &'static str,
    tauri: bool,
    secure_storage: bool,
    notifications: bool,
    microphone: bool,
    native_call_ui: bool,
    realtime: bool,
    media_bridge: bool,
    demo: bool,
    local_pilot: bool,
    notification_permission: String,
    microphone_permission: String,
    fcm_configured: bool,
    fcm_token_present: bool,
    push_registration: String,
    pending_call_offer: bool,
    battery_optimizations_restricted: bool,
    force_stop_state: String,
    call_infrastructure: String,
    last_native_diagnostic: Option<String>,
}

#[tauri::command]
async fn runtime_capabilities(
    app: AppHandle,
    media: State<'_, NativeMediaState>,
    managed_auth: State<'_, ManagedAuthState>,
) -> Result<RuntimeCapabilities, String> {
    let media_ready = media.ensure_platform_ready(&app).await;
    let android = if cfg!(target_os = "android") {
        android_runtime::diagnostics(&app).await.ok()
    } else {
        None
    };
    let secure_storage = match android.as_ref() {
        Some(diagnostics) => diagnostics.secure_storage,
        None => managed_auth.secure_storage_available(&app).await,
    };
    Ok(RuntimeCapabilities {
        platform: std::env::consts::OS,
        tauri: true,
        // These become true only when the platform VoIP plugin reports a real
        // implementation. The WebView must never infer them from OS names.
        secure_storage,
        notifications: android
            .as_ref()
            .is_some_and(|diagnostics| diagnostics.notifications_enabled),
        microphone: android.as_ref().map_or(media_ready, |diagnostics| {
            diagnostics.microphone_permission == "granted"
        }),
        native_call_ui: android
            .as_ref()
            .is_some_and(|diagnostics| diagnostics.native_call_ui),
        realtime: true,
        media_bridge: media_ready,
        demo: false,
        local_pilot: cfg!(feature = "managed-beta-local"),
        notification_permission: android.as_ref().map_or_else(
            || "not_applicable".into(),
            |value| value.notification_permission.clone(),
        ),
        microphone_permission: android.as_ref().map_or_else(
            || "unknown".into(),
            |value| value.microphone_permission.clone(),
        ),
        fcm_configured: android.as_ref().is_some_and(|value| value.fcm_configured),
        fcm_token_present: android
            .as_ref()
            .is_some_and(|value| value.fcm_token_present),
        push_registration: android.as_ref().map_or_else(
            || "not_applicable".into(),
            |value| value.push_registration.clone(),
        ),
        pending_call_offer: android
            .as_ref()
            .is_some_and(|value| value.pending_call_offer),
        battery_optimizations_restricted: android
            .as_ref()
            .is_some_and(|value| value.battery_optimizations_restricted),
        force_stop_state: android.as_ref().map_or_else(
            || "not_detectable".into(),
            |value| value.force_stop_state.clone(),
        ),
        call_infrastructure: android.as_ref().map_or_else(
            || "not_applicable".into(),
            |value| value.call_infrastructure.clone(),
        ),
        last_native_diagnostic: android.and_then(|value| value.last_native_diagnostic),
    })
}

#[tauri::command]
async fn request_notification_permission(app: AppHandle) -> Result<bool, String> {
    android_runtime::request_notification_permission(&app).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(RealtimeState::default())
        .manage(NativeMediaState::default())
        .manage(ManagedAuthState::default())
        .manage(DesktopPairingState::default())
        .manage(PeerTrustState::default())
        .manage(ServerProfileState::default())
        .invoke_handler(tauri::generate_handler![
            runtime_capabilities,
            discover_deployment,
            managed_authorize,
            managed_restore,
            managed_forget,
            companion_bootstrap,
            companion_history,
            companion_routing,
            companion_call_records,
            companion_call_record_detail,
            companion_availability,
            companion_set_availability,
            request_notification_permission,
            native_review_desktop_pairing_offer,
            native_confirm_desktop_pairing,
            native_confirm_desktop_peer_trust,
            native_begin_custom_server_authorization,
            native_confirm_custom_server_trust,
            native_connect_profile,
            native_list_server_profiles,
            native_rotate_server_trust,
            native_forget_server_profile,
            realtime_connect,
            realtime_disconnect,
            realtime_send,
            realtime_v2_request_lease,
            realtime_v2_revoke_lease,
            realtime_v2_answer_assistance,
            realtime_v2_prepare_end_caller,
            realtime_v2_confirm_end_caller,
            media_create_offer,
            media_get_audio_devices,
            media_select_audio_devices,
            media_accept_answer,
            media_add_ice_candidate,
            media_arm_microphone,
            media_disarm_microphone,
            media_renew_lease,
            media_revoke,
            media_close,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Aokie Companion");
}

// Keep the exported command response types referenced in this module so the
// generated mobile bindings remain stable when platform crates are enabled.
#[allow(dead_code)]
fn _wire_types(_: DiscoveryDocument, _: RealtimeConfig) {}
