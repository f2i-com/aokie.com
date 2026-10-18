//! Narrow Android native bridge.
//!
//! Refresh credentials cross only the Rust/JNI boundary into an AES-GCM value
//! protected by Android Keystore. They are never returned by a Tauri command.

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AndroidRuntimeDiagnostics {
    pub(crate) secure_storage: bool,
    pub(crate) notification_permission: String,
    /// Android will not show the notification dialog again: only the app's notification settings can allow them.
    #[serde(default)]
    pub(crate) notification_permission_blocked: bool,
    pub(crate) microphone_permission: String,
    pub(crate) notifications_enabled: bool,
    pub(crate) native_call_ui: bool,
    pub(crate) fcm_configured: bool,
    pub(crate) fcm_token_present: bool,
    pub(crate) push_registration: String,
    pub(crate) pending_call_offer: bool,
    pub(crate) battery_optimizations_restricted: bool,
    pub(crate) force_stop_state: String,
    pub(crate) call_infrastructure: String,
    pub(crate) last_native_diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidAuthoritativeOffer {
    pub(crate) schema_version: u16,
    pub(crate) event_id: String,
    pub(crate) offer_id: String,
    pub(crate) opportunity_id: String,
    pub(crate) app_id: String,
    pub(crate) call_id: String,
    pub(crate) call_epoch: u64,
    pub(crate) owner_epoch: u64,
    pub(crate) expires_at: u64,
    pub(crate) accepted_transfer_request_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AndroidAudioRoute {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) label: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AndroidAudioRoutes {
    schema_version: u16,
    pub(crate) revision: u64,
    pub(crate) routes: Vec<AndroidAudioRoute>,
    pub(crate) selected_id: String,
    pub(crate) can_select: bool,
    pub(crate) state: String,
}

impl AndroidAudioRoutes {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || self.revision == 0
            || self.revision > 9_007_199_254_740_991
            || self.routes.len() > 32
            || !matches!(self.state.as_str(), "idle" | "media_active")
            || (self.can_select != (self.state == "media_active" && !self.routes.is_empty()))
        {
            return Err("Android communication-route snapshot is invalid".into());
        }
        let mut ids = std::collections::HashSet::new();
        for route in &self.routes {
            if !safe_native_id(&route.id)
                || !matches!(
                    route.kind.as_str(),
                    "speaker" | "earpiece" | "wired" | "bluetooth"
                )
                || route.label.is_empty()
                || route.label.len() > 120
                || route.label.chars().any(char::is_control)
                || !ids.insert(route.id.as_str())
            {
                return Err("Android communication route is invalid".into());
            }
        }
        if self.selected_id != "system_managed" && !ids.contains(self.selected_id.as_str()) {
            return Err("Android selected communication route is unavailable".into());
        }
        Ok(())
    }
}

#[cfg(target_os = "android")]
pub(crate) async fn diagnostics(app: &AppHandle) -> Result<AndroidRuntimeDiagnostics, String> {
    use jni::objects::JString;
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<String, String> {
                    let value = env
                        .call_method(
                            activity,
                            "aokieRuntimeDiagnostics",
                            "()Ljava/lang/String;",
                            &[],
                        )
                        .map_err(|error| jni_failure(env, error))?
                        .l()
                        .map_err(|error| jni_failure(env, error))?;
                    if value.is_null() {
                        return Err("Android diagnostics returned no value".into());
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    let encoded = tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android diagnostics timed out".to_string())?
        .map_err(|_| "Android diagnostics were cancelled".to_string())??;
    serde_json::from_str(&encoded).map_err(|_| "Android diagnostics were invalid".into())
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn diagnostics(_app: &AppHandle) -> Result<AndroidRuntimeDiagnostics, String> {
    Err("Android runtime is unavailable on this platform".into())
}

#[cfg(target_os = "android")]
pub(crate) async fn secure_store_available(app: &AppHandle) -> bool {
    call_int_no_args(app, "aokieSecureStoreAvailable")
        .await
        .is_ok_and(|value| value > 0)
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn secure_store_available(_app: &AppHandle) -> bool {
    false
}

#[cfg(target_os = "android")]
pub(crate) async fn secure_store_put(
    app: &AppHandle,
    key: &str,
    value: &str,
) -> Result<(), String> {
    use jni::objects::{JObject, JValue};
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let key = key.to_owned();
    let value = value.to_owned();
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<i32, String> {
                    let key =
                        JObject::from(env.new_string(key).map_err(|error| jni_failure(env, error))?);
                    let value =
                        JObject::from(env.new_string(value).map_err(|error| jni_failure(env, error))?);
                    env.call_method(
                        activity,
                        "putAokieSecureValue",
                        "(Ljava/lang/String;Ljava/lang/String;)I",
                        &[JValue::Object(&key), JValue::Object(&value)],
                    )
                    .and_then(|value| value.i())
                    .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android secure-store write timed out".to_string())?
        .map_err(|_| "Android secure-store write was cancelled".to_string())??;
    if result > 0 {
        Ok(())
    } else {
        Err("Android Keystore-backed write failed".into())
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn secure_store_put(
    _app: &AppHandle,
    _key: &str,
    _value: &str,
) -> Result<(), String> {
    Err("Android secure storage is unavailable on this platform".into())
}

#[cfg(target_os = "android")]
pub(crate) async fn secure_store_get(app: &AppHandle, key: &str) -> Result<Option<String>, String> {
    use jni::objects::{JObject, JString, JValue};
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let key = key.to_owned();
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<Option<String>, String> {
                    let key =
                        JObject::from(env.new_string(key).map_err(|error| jni_failure(env, error))?);
                    // `null` means nothing is stored. A value that exists but cannot be read
                    // throws (see MainActivity.getAokieSecureValue) and arrives here as an
                    // error, never as absence: callers must not replace what they cannot read.
                    let value = env
                        .call_method(
                            activity,
                            "getAokieSecureValue",
                            "(Ljava/lang/String;)Ljava/lang/String;",
                            &[JValue::Object(&key)],
                        )
                        .map_err(|error| {
                            format!(
                                "Android secure-store value could not be read ({})",
                                jni_failure(env, error)
                            )
                        })?
                        .l()
                        .map_err(|error| jni_failure(env, error))?;
                    if value.is_null() {
                        return Ok(None);
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map(Some)
                        .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android secure-store read timed out".to_string())?
        .map_err(|_| "Android secure-store read was cancelled".to_string())?
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn secure_store_get(
    _app: &AppHandle,
    _key: &str,
) -> Result<Option<String>, String> {
    Err("Android secure storage is unavailable on this platform".into())
}

#[cfg(target_os = "android")]
pub(crate) async fn secure_store_delete(app: &AppHandle, key: &str) -> Result<(), String> {
    use jni::objects::{JObject, JValue};
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let key = key.to_owned();
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<i32, String> {
                    let key =
                        JObject::from(env.new_string(key).map_err(|error| jni_failure(env, error))?);
                    env.call_method(
                        activity,
                        "deleteAokieSecureValue",
                        "(Ljava/lang/String;)I",
                        &[JValue::Object(&key)],
                    )
                    .and_then(|value| value.i())
                    .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    let value = tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android secure-store delete timed out".to_string())?
        .map_err(|_| "Android secure-store delete was cancelled".to_string())??;
    if value > 0 {
        Ok(())
    } else {
        Err("Android Keystore-backed delete failed".into())
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn secure_store_delete(_app: &AppHandle, _key: &str) -> Result<(), String> {
    Err("Android secure storage is unavailable on this platform".into())
}

#[cfg(target_os = "android")]
pub(crate) async fn present_authoritative_offer(
    app: &AppHandle,
    offer: &AndroidAuthoritativeOffer,
) -> Result<bool, String> {
    let encoded = serde_json::to_string(offer)
        .map_err(|_| "Android authoritative offer could not be encoded".to_string())?;
    if encoded.len() > 4 * 1024 {
        return Err("Android authoritative offer exceeded its size limit".into());
    }
    Ok(call_int_string(app, "presentAokieAuthoritativeOffer", encoded).await? > 0)
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn present_authoritative_offer(
    _app: &AppHandle,
    _offer: &AndroidAuthoritativeOffer,
) -> Result<bool, String> {
    Ok(false)
}

#[cfg(target_os = "android")]
pub(crate) async fn take_native_call_action(app: &AppHandle) -> Result<Option<String>, String> {
    use jni::objects::JString;
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<Option<String>, String> {
                    let value = env
                        .call_method(
                            activity,
                            "takeAokieNativeCallAction",
                            "()Ljava/lang/String;",
                            &[],
                        )
                        .map_err(|error| jni_failure(env, error))?
                        .l()
                        .map_err(|error| jni_failure(env, error))?;
                    if value.is_null() {
                        return Ok(None);
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map(Some)
                        .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(2), receive)
        .await
        .map_err(|_| "Android native-call action read timed out".to_string())?
        .map_err(|_| "Android native-call action read was cancelled".to_string())?
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn take_native_call_action(_app: &AppHandle) -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(target_os = "android")]
pub(crate) async fn complete_native_call_action(
    app: &AppHandle,
    action_id: &str,
    accepted: bool,
    code: &str,
) -> Result<(), String> {
    use jni::objects::{JObject, JValue};
    use tauri::Manager;
    use tokio::sync::oneshot;

    if !safe_native_id(action_id)
        || code.is_empty()
        || code.len() > 120
        || code.chars().any(char::is_control)
    {
        return Err("Android native-call result is invalid".into());
    }
    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let action_id = action_id.to_owned();
    let code = code.to_owned();
    let accepted = i32::from(accepted);
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<i32, String> {
                    let action_id = JObject::from(
                        env.new_string(action_id)
                            .map_err(|error| jni_failure(env, error))?,
                    );
                    let code =
                        JObject::from(env.new_string(code).map_err(|error| jni_failure(env, error))?);
                    env.call_method(
                        activity,
                        "completeAokieNativeCallAction",
                        "(Ljava/lang/String;ILjava/lang/String;)I",
                        &[
                            JValue::Object(&action_id),
                            JValue::Int(accepted),
                            JValue::Object(&code),
                        ],
                    )
                    .and_then(|value| value.i())
                    .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), receive)
        .await
        .map_err(|_| "Android native-call result timed out".to_string())?
        .map_err(|_| "Android native-call result was cancelled".to_string())??;
    if result > 0 {
        Ok(())
    } else {
        Err("Android native-call result was rejected".into())
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn complete_native_call_action(
    _app: &AppHandle,
    _action_id: &str,
    _accepted: bool,
    _code: &str,
) -> Result<(), String> {
    Ok(())
}

/// The error text of a failed bridge call, after clearing any Java exception it left pending.
///
/// A Kotlin method that throws leaves its exception pending in this JNI environment, and wry hands the
/// environment straight back to Android's main looper: the next JNI call there, or the looper's return
/// to Java, then aborts the app mid-call. The Kotlin bridge methods catch their own failures, so this is
/// the backstop; the one expected exception is a secure-store read error, thrown on purpose. Every JNI
/// call in this file routes its error through here, and so must any new one (media.rs's permission
/// bridge included).
#[cfg(target_os = "android")]
pub(crate) fn jni_failure(env: &mut jni::JNIEnv, error: jni::errors::Error) -> String {
    if env.exception_check().unwrap_or(false) {
        // Logs the Java stack trace to logcat; the messages carry no secret values.
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    error.to_string()
}

fn safe_native_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

#[cfg(target_os = "android")]
pub(crate) async fn request_notification_permission(app: &AppHandle) -> Result<bool, String> {
    const REQUEST_ID: i32 = 4_218;
    let initial = call_int_i32(app, "requestAokieNotificationPermission", REQUEST_ID).await?;
    if initial > 0 {
        return Ok(true);
    }
    if initial < 0 {
        return Ok(false);
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let result = call_int_i32(app, "pollAokieNotificationPermission", REQUEST_ID).await?;
        if result != 0 {
            return Ok(result > 0);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("notification permission request timed out".into());
        }
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn request_notification_permission(_app: &AppHandle) -> Result<bool, String> {
    Ok(false)
}

/// Opens the app's notification settings (Android). False when this device has no such screen.
#[cfg(target_os = "android")]
pub(crate) async fn open_notification_settings(app: &AppHandle) -> Result<bool, String> {
    Ok(call_int_no_args(app, "openAokieNotificationSettings").await? > 0)
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn open_notification_settings(_app: &AppHandle) -> Result<bool, String> {
    Ok(false)
}

#[cfg(target_os = "android")]
pub(crate) async fn reconcile_offer(
    app: &AppHandle,
    call_id: &str,
    call_epoch: u64,
    outcome: &str,
    reason: &str,
) -> Result<bool, String> {
    use jni::objects::{JObject, JValue};
    use tauri::Manager;
    use tokio::sync::oneshot;

    if !matches!(outcome, "won" | "cancel")
        || call_epoch == 0
        || call_epoch > 9_007_199_254_740_991
        || reason.len() > 120
        || reason.chars().any(char::is_control)
    {
        return Err("Android offer reconciliation is invalid".into());
    }
    let call_epoch = i64::try_from(call_epoch).map_err(|_| "call epoch is invalid")?;
    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let call_id = call_id.to_owned();
    let outcome = outcome.to_owned();
    let reason = reason.to_owned();
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<i32, String> {
                    let call_id =
                        JObject::from(env.new_string(call_id).map_err(|error| jni_failure(env, error))?);
                    let outcome =
                        JObject::from(env.new_string(outcome).map_err(|error| jni_failure(env, error))?);
                    let reason =
                        JObject::from(env.new_string(reason).map_err(|error| jni_failure(env, error))?);
                    env.call_method(
                        activity,
                        "reconcileAokieOffer",
                        "(Ljava/lang/String;JLjava/lang/String;Ljava/lang/String;)I",
                        &[
                            JValue::Object(&call_id),
                            JValue::Long(call_epoch),
                            JValue::Object(&outcome),
                            JValue::Object(&reason),
                        ],
                    )
                    .and_then(|value| value.i())
                    .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android offer reconciliation timed out".to_string())?
        .map_err(|_| "Android offer reconciliation was cancelled".to_string())??;
    Ok(result > 0)
}

#[cfg(target_os = "android")]
pub(crate) async fn invalidate_push_token(app: &AppHandle) -> Result<(), String> {
    if call_int_no_args(app, "invalidateAokiePushToken").await? > 0 {
        Ok(())
    } else {
        Err("Android push token could not be invalidated".into())
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn invalidate_push_token(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "android")]
pub(crate) async fn begin_communication_audio(app: &AppHandle) -> Result<(), String> {
    if call_int_no_args(app, "beginAokieCommunicationAudio").await? > 0 {
        Ok(())
    } else {
        Err("Android communication audio could not start".into())
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn begin_communication_audio(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "android")]
pub(crate) async fn end_communication_audio(app: &AppHandle) -> Result<(), String> {
    if call_int_no_args(app, "endAokieCommunicationAudio").await? > 0 {
        Ok(())
    } else {
        Err("Android communication audio could not close".into())
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn end_communication_audio(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "android")]
pub(crate) async fn audio_routes(app: &AppHandle) -> Result<AndroidAudioRoutes, String> {
    decode_audio_routes(&call_string_no_args(app, "aokieAudioRoutes").await?)
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn audio_routes(_app: &AppHandle) -> Result<AndroidAudioRoutes, String> {
    Err("Android communication routes are unavailable on this platform".into())
}

#[cfg(target_os = "android")]
pub(crate) async fn select_audio_route(
    app: &AppHandle,
    route_id: &str,
) -> Result<AndroidAudioRoutes, String> {
    if !safe_native_id(route_id) {
        return Err("Android communication route identity is invalid".into());
    }
    decode_audio_routes(
        &call_string_string(app, "selectAokieAudioRoute", route_id.to_owned()).await?,
    )
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn select_audio_route(
    _app: &AppHandle,
    _route_id: &str,
) -> Result<AndroidAudioRoutes, String> {
    Err("Android communication routes are unavailable on this platform".into())
}

#[cfg(target_os = "android")]
pub(crate) async fn signal_live_transition(app: &AppHandle) -> Result<(), String> {
    if call_int_no_args(app, "signalAokieLiveTransition").await? > 0 {
        Ok(())
    } else {
        Err("Android live-transition cue could not be presented".into())
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn signal_live_transition(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

fn decode_audio_routes(encoded: &str) -> Result<AndroidAudioRoutes, String> {
    if encoded.is_empty() || encoded.len() > 16 * 1024 {
        return Err("Android communication-route snapshot is invalid".into());
    }
    let routes: AndroidAudioRoutes = serde_json::from_str(encoded)
        .map_err(|_| "Android communication-route snapshot is malformed".to_string())?;
    routes.validate()?;
    Ok(routes)
}

#[cfg(not(target_os = "android"))]
pub(crate) async fn reconcile_offer(
    _app: &AppHandle,
    _call_id: &str,
    _call_epoch: u64,
    _outcome: &str,
    _reason: &str,
) -> Result<bool, String> {
    Ok(false)
}

#[cfg(target_os = "android")]
async fn call_int_no_args(app: &AppHandle, method: &'static str) -> Result<i32, String> {
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = env
                    .call_method(activity, method, "()I", &[])
                    .and_then(|value| value.i())
                    .map_err(|error| jni_failure(env, error));
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android native bridge timed out".to_string())?
        .map_err(|_| "Android native bridge was cancelled".to_string())?
}

#[cfg(target_os = "android")]
async fn call_int_i32(app: &AppHandle, method: &'static str, argument: i32) -> Result<i32, String> {
    use jni::objects::JValue;
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = env
                    .call_method(activity, method, "(I)I", &[JValue::Int(argument)])
                    .and_then(|value| value.i())
                    .map_err(|error| jni_failure(env, error));
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android native bridge timed out".to_string())?
        .map_err(|_| "Android native bridge was cancelled".to_string())?
}

#[cfg(target_os = "android")]
async fn call_string_no_args(app: &AppHandle, method: &'static str) -> Result<String, String> {
    use jni::objects::JString;
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<String, String> {
                    let value = env
                        .call_method(activity, method, "()Ljava/lang/String;", &[])
                        .map_err(|error| jni_failure(env, error))?
                        .l()
                        .map_err(|error| jni_failure(env, error))?;
                    if value.is_null() {
                        return Err("Android native bridge returned no value".into());
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android native string bridge timed out".to_string())?
        .map_err(|_| "Android native string bridge was cancelled".to_string())?
}

#[cfg(target_os = "android")]
async fn call_int_string(
    app: &AppHandle,
    method: &'static str,
    argument: String,
) -> Result<i32, String> {
    use jni::objects::{JObject, JValue};
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<i32, String> {
                    let argument = JObject::from(
                        env.new_string(argument)
                            .map_err(|error| jni_failure(env, error))?,
                    );
                    env.call_method(
                        activity,
                        method,
                        "(Ljava/lang/String;)I",
                        &[JValue::Object(&argument)],
                    )
                    .and_then(|value| value.i())
                    .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android native string call timed out".to_string())?
        .map_err(|_| "Android native string call was cancelled".to_string())?
}

#[cfg(target_os = "android")]
async fn call_string_string(
    app: &AppHandle,
    method: &'static str,
    argument: String,
) -> Result<String, String> {
    use jni::objects::{JObject, JString, JValue};
    use tauri::Manager;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<String, String> {
                    let argument = JObject::from(
                        env.new_string(argument)
                            .map_err(|error| jni_failure(env, error))?,
                    );
                    let value = env
                        .call_method(
                            activity,
                            method,
                            "(Ljava/lang/String;)Ljava/lang/String;",
                            &[JValue::Object(&argument)],
                        )
                        .map_err(|error| jni_failure(env, error))?
                        .l()
                        .map_err(|error| jni_failure(env, error))?;
                    if value.is_null() {
                        return Err("Android native bridge returned no value".into());
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map_err(|error| jni_failure(env, error))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android native string bridge timed out".to_string())?
        .map_err(|_| "Android native string bridge was cancelled".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_contract_is_strict_and_contains_no_token() {
        let encoded = r#"{
          "secureStorage":true,
          "notificationPermission":"granted",
          "notificationPermissionBlocked":false,
          "microphonePermission":"denied",
          "notificationsEnabled":true,
          "nativeCallUi":true,
          "fcmConfigured":false,
          "fcmTokenPresent":false,
          "pushRegistration":"configuration_required",
          "pendingCallOffer":false,
          "batteryOptimizationsRestricted":true,
          "forceStopState":"not_detectable",
          "callInfrastructure":"ready_for_authoritative_offers",
          "lastNativeDiagnostic":null
        }"#;
        let value: AndroidRuntimeDiagnostics = serde_json::from_str(encoded).unwrap();
        assert!(value.secure_storage);
        assert!(!value.fcm_token_present);
        assert!(!encoded.contains("refresh_token"));
    }

    #[test]
    fn diagnostics_from_an_older_native_layer_still_decode() {
        // `notificationPermissionBlocked` is new; a build whose Kotlin does not send it reads as "not blocked".
        let encoded = r#"{
          "secureStorage":true,"notificationPermission":"denied","microphonePermission":"denied",
          "notificationsEnabled":false,"nativeCallUi":false,"fcmConfigured":false,"fcmTokenPresent":false,
          "pushRegistration":"configuration_required","pendingCallOffer":false,
          "batteryOptimizationsRestricted":false,"forceStopState":"not_detectable",
          "callInfrastructure":"notification_permission_required","lastNativeDiagnostic":null
        }"#;
        let value: AndroidRuntimeDiagnostics = serde_json::from_str(encoded).unwrap();
        assert!(!value.notification_permission_blocked);
        let blocked = encoded.replace(
            "\"notificationPermission\":\"denied\",",
            "\"notificationPermission\":\"denied\",\"notificationPermissionBlocked\":true,",
        );
        assert!(
            serde_json::from_str::<AndroidRuntimeDiagnostics>(&blocked)
                .unwrap()
                .notification_permission_blocked
        );
    }

    #[test]
    fn the_notification_settings_bridge_is_declared_and_kept_from_r8() {
        // Rust finds `openAokieNotificationSettings` by name through JNI, so R8 must neither remove nor rename it.
        const MAIN_ACTIVITY: &str =
            include_str!("../gen/android/app/src/main/java/com/aokie/companion/MainActivity.kt");
        const PROGUARD: &str = include_str!("../gen/android/app/proguard-rules.pro");
        assert!(MAIN_ACTIVITY.contains("fun openAokieNotificationSettings(): Int"));
        assert!(PROGUARD.contains("public int openAokieNotificationSettings();"));
        assert!(include_str!("android_runtime.rs").contains("\"openAokieNotificationSettings\""));
    }

    const KOTLIN_DIR_MAIN_ACTIVITY: &str =
        include_str!("../gen/android/app/src/main/java/com/aokie/companion/MainActivity.kt");
    const KOTLIN_SECURE_STORE: &str =
        include_str!("../gen/android/app/src/main/java/com/aokie/companion/AokieSecureStore.kt");
    const KOTLIN_NOTIFICATIONS: &str =
        include_str!("../gen/android/app/src/main/java/com/aokie/companion/AokieCallNotifications.kt");
    const KOTLIN_CALL_SERVICE: &str = include_str!(
        "../gen/android/app/src/main/java/com/aokie/companion/AokieIncomingCallService.kt"
    );
    const KOTLIN_OFFER_STORE: &str =
        include_str!("../gen/android/app/src/main/java/com/aokie/companion/AokieOfferStore.kt");
    const KOTLIN_FCM: &str = include_str!(
        "../gen/android/app/src/main/java/com/aokie/companion/AokieFirebaseMessagingService.kt"
    );
    const KOTLIN_AUDIO_ROUTES: &str =
        include_str!("../gen/android/app/src/main/java/com/aokie/companion/AokieAudioRoutes.kt");
    const KOTLIN_COMMUNICATION_SERVICE: &str = include_str!(
        "../gen/android/app/src/main/java/com/aokie/companion/AokieCommunicationService.kt"
    );
    const MANIFEST: &str = include_str!("../gen/android/app/src/main/AndroidManifest.xml");
    const PROGUARD_RULES: &str = include_str!("../gen/android/app/proguard-rules.pro");
    const GRADLE: &str = include_str!("../gen/android/app/build.gradle.kts");

    /// The Kotlin methods Rust calls by name through JNI: every string literal in `sources` that is a
    /// lowerCamelCase identifier naming Aokie (`takeAokieNativeCallAction`, `aokieAudioRoutes`).
    fn jni_method_names(sources: &[&str]) -> std::collections::BTreeSet<String> {
        let mut names = std::collections::BTreeSet::new();
        for source in sources {
            let mut rest = *source;
            while let Some(quote) = rest.find('"') {
                rest = &rest[quote + 1..];
                let length = rest
                    .bytes()
                    .take_while(u8::is_ascii_alphanumeric)
                    .count();
                if length == 0 || rest.as_bytes().get(length) != Some(&b'"') {
                    continue;
                }
                let name = &rest[..length];
                let bytes = name.as_bytes();
                let bridge = bytes[0].is_ascii_lowercase()
                    && (name.contains("Aokie")
                        || (name.starts_with("aokie")
                            && bytes.get(5).is_some_and(u8::is_ascii_uppercase)));
                if bridge {
                    names.insert(name.to_owned());
                }
            }
        }
        names
    }

    /// The body of proguard-rules.pro's keep block for MainActivity's bridge methods.
    fn bridge_keep_block() -> &'static str {
        PROGUARD_RULES
            .split("-keepclassmembers class com.aokie.companion.MainActivity {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("proguard-rules.pro keeps MainActivity's bridge methods")
    }

    #[test]
    fn every_kotlin_method_rust_calls_is_declared_and_kept_from_r8() {
        // Release builds minify with R8, which cannot see JNI use: a bridge method missing from the keep
        // list is stripped, and the first call to it (the 200 ms native-call poll among them) dies with
        // NoSuchMethodError.
        let names = jni_method_names(&[
            include_str!("android_runtime.rs"),
            include_str!("media.rs"),
            include_str!("native_init.rs"),
            include_str!("realtime_v2.rs"),
            include_str!("realtime.rs"),
            include_str!("managed_auth.rs"),
            include_str!("endpoint_identity.rs"),
            include_str!("push_registration.rs"),
            include_str!("lib.rs"),
        ]);
        for known in [
            "takeAokieNativeCallAction",
            "aokieAudioRoutes",
            "signalAokieLiveTransition",
            "requestAokieMicrophonePermission",
        ] {
            assert!(names.contains(known), "the scan missed `{known}`");
        }
        let keep = bridge_keep_block();
        for name in &names {
            assert!(
                KOTLIN_DIR_MAIN_ACTIVITY.contains(&format!("fun {name}(")),
                "Rust calls `{name}`, which MainActivity.kt does not declare"
            );
            assert!(
                keep.contains(&format!(" {name}(")),
                "proguard-rules.pro does not keep MainActivity.{name}: release R8 would strip it"
            );
        }
        // The keep list names nothing MainActivity no longer has.
        for line in keep
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("public ") && !line.contains('*'))
        {
            let name = line
                .split('(')
                .next()
                .and_then(|head| head.rsplit(' ').next())
                .expect("a kept method has a name");
            assert!(
                KOTLIN_DIR_MAIN_ACTIVITY.contains(&format!("fun {name}(")),
                "proguard-rules.pro keeps `{name}`, which MainActivity.kt does not declare"
            );
        }
        // The post-R8 check reads the same block and also checks members.
        assert!(GRADLE.contains("val jniBridgeClass = \"com.aokie.companion.MainActivity\""));
        assert!(GRADLE.contains("usage.txt"));
    }

    #[test]
    fn the_name_scan_finds_bridge_calls_and_nothing_else() {
        let names = jni_method_names(&[
            r#"call(app, "takeAokieNativeCallAction"); x("aokieAudioRoutes", "AokieProbe")"#,
            r#""aokie-companion://v2-idle-sync" "plain" "aokiecase" "", "#,
        ]);
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            ["aokieAudioRoutes", "takeAokieNativeCallAction"]
        );
    }

    #[test]
    fn every_jni_error_path_clears_the_pending_java_exception() {
        // An exception left pending after a bridge call aborts the app at the next JNI call on the main
        // looper. Inside a JNI closure (indented past the async function body) an error must go through
        // `jni_failure`; only the Tauri `with_webview` errors, at the function's own indent, may not.
        let source = include_str!("android_runtime.rs");
        let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
        for (number, line) in production.lines().enumerate() {
            let indent = line.len() - line.trim_start().len();
            if indent > 8 && line.contains("error.to_string()") {
                panic!(
                    "android_runtime.rs:{} maps a JNI error without clearing the Java exception",
                    number + 1
                );
            }
        }
        assert!(production.matches("jni_failure(env, error)").count() >= 30);
        assert!(production.contains("env.exception_clear()"));
        // The Kotlin side catches its own failures in every bridge method.
        for method in [
            "fun takeAokieNativeCallAction(): String? =\n    runCatching",
            "fun selectAokieAudioRoute(routeId: String): String? =\n    runCatching",
            "fun presentAokieAuthoritativeOffer(encoded: String): Int = runCatching",
            "fun reconcileAokieOffer(callId: String, callEpoch: Long, outcome: String, reason: String): Int = runCatching",
            "fun aokieRuntimeDiagnostics(): String? = runCatching",
            "fun requestAokieMicrophonePermission(requestId: Int): Int = runCatching",
        ] {
            assert!(
                KOTLIN_DIR_MAIN_ACTIVITY.contains(method),
                "MainActivity.kt lost `{method}`"
            );
        }
    }

    #[test]
    fn a_secure_value_that_cannot_be_read_is_an_error_not_absence() {
        // Rust reads `null` as "nothing stored": returning it for a Keystore or decrypt error made the
        // caller mint a new identity, sign the user out, or rewrite the profile index.
        assert!(KOTLIN_SECURE_STORE.contains("fun read(context: Context, logicalKey: String): Result<String?>"));
        assert!(KOTLIN_SECURE_STORE.contains("if (!storedFileExists(target)) return@runCatching null"));
        // Before API 30 a write killed midway leaves only AtomicFile's backup;
        // it is still a stored value, never absence.
        assert!(KOTLIN_SECURE_STORE.contains("target.exists() || java.io.File(target.path + \".bak\").exists()"));
        assert!(KOTLIN_DIR_MAIN_ACTIVITY.contains("AokieSecureStore.read(this, key).getOrElse {"));
        assert!(KOTLIN_DIR_MAIN_ACTIVITY.contains("throw IllegalStateException(\"Aokie secure value could not be read\")"));
        assert!(include_str!("android_runtime.rs")
            .contains("\"Android secure-store value could not be read ({})\""));
    }

    #[test]
    fn a_lost_keystore_key_resets_the_store_instead_of_failing_forever() {
        // Only definite loss resets: a missing key (its values can never be
        // read again), an unrecoverable key, or a permanently invalidated one.
        let key = KOTLIN_SECURE_STORE
            .split("private fun key(context: Context): SecretKey {")
            .nth(1)
            .and_then(|rest| rest.split("val generator").next())
            .expect("the store derives its key");
        assert!(key.contains("catch (error: UnrecoverableKeyException)"));
        assert!(key.contains("resetUnreadableValues(context)"));
        assert!(!key.contains("catch (error: Exception)") && !key.contains("catch (error: Throwable)"));
        assert_eq!(
            KOTLIN_SECURE_STORE
                .matches("catch (error: KeyPermanentlyInvalidatedException)")
                .count(),
            2
        );
        // A read asks for the key first, so values a lost key sealed read as
        // absent rather than as a decrypt error on every launch.
        let read = KOTLIN_SECURE_STORE
            .split("fun read(context: Context, logicalKey: String)")
            .nth(1)
            .expect("the store reads");
        assert!(
            read.find("val secret = key(context)").expect("read asks for the key")
                < read.find("storedFileExists(target)").expect("read checks for the value")
        );
    }

    #[test]
    fn a_push_only_answer_waits_for_its_signed_offer_until_the_answer_deadline() {
        // The app's own Answer waits the whole claim window, not five seconds;
        // a system surface keeps the short wait Telecom's callback allows.
        assert!(KOTLIN_CALL_SERVICE.contains("waitForDeadline: Boolean = true"));
        assert!(KOTLIN_CALL_SERVICE.contains("aokieAuthoritativeOfferWaitMs("));
        assert!(KOTLIN_CALL_SERVICE.contains(".coerceIn(0L, AokieVoiceOffer.ANSWER_SETUP_TIMEOUT_SECONDS * 1_000)"));
        assert!(!KOTLIN_CALL_SERVICE.contains("elapsedRealtime() + 5_000L"));
    }

    #[test]
    fn the_notification_answer_opens_the_app_and_runs_the_system_answer_claim() {
        assert!(KOTLIN_NOTIFICATIONS.contains(".setAction(MainActivity.ACTION_ANSWER_OFFER)"));
        assert!(
            !KOTLIN_NOTIFICATIONS.contains("AokieIncomingCallService.ACTION_ANSWER, 3)"),
            "Answer must not be a service PendingIntent: Android 12+ blocks the app launch it needs"
        );
        assert!(KOTLIN_DIR_MAIN_ACTIVITY.contains("intent?.action == ACTION_ANSWER_OFFER"));
        assert!(KOTLIN_DIR_MAIN_ACTIVITY.contains(".setAction(AokieIncomingCallService.ACTION_ANSWER)"));
        // Both Answer paths share the claim, and the in-app one runs it before Telecom's answer().
        let local = KOTLIN_CALL_SERVICE
            .split("is LocalAction.Answer -> {")
            .nth(1)
            .and_then(|rest| rest.split("is LocalAction.Won -> {").next())
            .expect("the service handles a local Answer");
        let claim = local.find("claimAnswer(offer, fence)").expect("local Answer claims");
        let answer = local
            .find("answer(CallAttributesCompat.CALL_TYPE_AUDIO_CALL)")
            .expect("local Answer answers");
        assert!(claim < answer);
        assert!(KOTLIN_CALL_SERVICE
            .contains("if (!claimAnswer(offer, fence, waitForDeadline = false)) {\n      throw"));
    }

    #[test]
    fn companion_audio_holds_a_microphone_foreground_service_while_it_runs() {
        assert!(MANIFEST.contains("android:name=\".AokieCommunicationService\""));
        assert!(MANIFEST.contains("android:foregroundServiceType=\"microphone|phoneCall\""));
        assert!(MANIFEST.contains("android.permission.FOREGROUND_SERVICE_MICROPHONE"));
        assert!(MANIFEST.contains("android.permission.WAKE_LOCK"));
        assert!(KOTLIN_AUDIO_ROUTES.contains("AokieCommunicationService.start(context)"));
        assert!(KOTLIN_AUDIO_ROUTES.contains("AokieCommunicationService.stop(context)"));
        // Started with a plain startService, so a refused foreground start can never crash the app.
        assert!(!KOTLIN_COMMUNICATION_SERVICE.contains("startForegroundService("));
        assert!(KOTLIN_COMMUNICATION_SERVICE.contains("PROXIMITY_SCREEN_OFF_WAKE_LOCK"));
    }

    #[test]
    fn pushes_never_replace_or_cancel_an_answered_call() {
        assert!(KOTLIN_OFFER_STORE.contains(
            "if (!aokiePushMayReplace(current, offer, AokieIncomingCallService.liveOfferId)) return@synchronized false"
        ));
        assert!(KOTLIN_FCM.contains("AokieOfferStore.cancelFromPush("));
        assert!(!KOTLIN_FCM.contains("AokieOfferStore.cancel(this, cancellation.offerId"));
        assert!(KOTLIN_OFFER_STORE.contains("authoritative_offer_refused_during_live_call"));
        // The call service publishes which call it runs, so a dead process's won record is recognised.
        assert!(KOTLIN_CALL_SERVICE.contains("liveOfferId = offer.offerId"));
    }

    #[test]
    fn native_call_action_ids_are_strict() {
        assert!(safe_native_id("6a672fd2-6499-4d5a-9c49-aokie"));
        assert!(!safe_native_id("contains space"));
        assert!(!safe_native_id("line\nbreak"));
        assert!(!safe_native_id(""));
    }

    #[test]
    fn android_audio_route_snapshot_accepts_active_system_routes() {
        let routes = decode_audio_routes(
            r#"{
              "schemaVersion":1,
              "revision":7,
              "routes":[
                {"id":"speaker:2","kind":"speaker","label":"Speaker"},
                {"id":"bluetooth:9","kind":"bluetooth","label":"Bluetooth headset"}
              ],
              "selectedId":"speaker:2",
              "canSelect":true,
              "state":"media_active"
            }"#,
        )
        .unwrap();
        assert_eq!(routes.selected_id, "speaker:2");
        assert_eq!(routes.routes.len(), 2);
    }

    #[test]
    fn android_audio_route_snapshot_rejects_forged_or_inconsistent_state() {
        let unavailable_selection = r#"{
          "schemaVersion":1,"revision":1,
          "routes":[{"id":"speaker:2","kind":"speaker","label":"Speaker"}],
          "selectedId":"bluetooth:9","canSelect":true,"state":"media_active"
        }"#;
        assert!(decode_audio_routes(unavailable_selection).is_err());

        let selectable_while_idle = r#"{
          "schemaVersion":1,"revision":1,
          "routes":[{"id":"speaker:2","kind":"speaker","label":"Speaker"}],
          "selectedId":"speaker:2","canSelect":true,"state":"idle"
        }"#;
        assert!(decode_audio_routes(selectable_while_idle).is_err());

        let unknown_field = r#"{
          "schemaVersion":1,"revision":1,"routes":[],
          "selectedId":"system_managed","canSelect":false,"state":"idle",
          "deviceAddress":"private-hardware-address"
        }"#;
        assert!(decode_audio_routes(unknown_field).is_err());
    }
}
