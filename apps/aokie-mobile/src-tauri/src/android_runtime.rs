//! Narrow Android native bridge.
//!
//! Refresh credentials cross only the Rust/JNI boundary into an AES-GCM value
//! protected by Android Keystore. They are never returned by a Tauri command.

use serde::Deserialize;
use tauri::AppHandle;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AndroidRuntimeDiagnostics {
    pub(crate) secure_storage: bool,
    pub(crate) notification_permission: String,
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
                        .map_err(|error| error.to_string())?
                        .l()
                        .map_err(|error| error.to_string())?;
                    if value.is_null() {
                        return Err("Android diagnostics returned no value".into());
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map_err(|error| error.to_string())
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
                        JObject::from(env.new_string(key).map_err(|error| error.to_string())?);
                    let value =
                        JObject::from(env.new_string(value).map_err(|error| error.to_string())?);
                    env.call_method(
                        activity,
                        "putAokieSecureValue",
                        "(Ljava/lang/String;Ljava/lang/String;)I",
                        &[JValue::Object(&key), JValue::Object(&value)],
                    )
                    .and_then(|value| value.i())
                    .map_err(|error| error.to_string())
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
                        JObject::from(env.new_string(key).map_err(|error| error.to_string())?);
                    let value = env
                        .call_method(
                            activity,
                            "getAokieSecureValue",
                            "(Ljava/lang/String;)Ljava/lang/String;",
                            &[JValue::Object(&key)],
                        )
                        .map_err(|error| error.to_string())?
                        .l()
                        .map_err(|error| error.to_string())?;
                    if value.is_null() {
                        return Ok(None);
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map(Some)
                        .map_err(|error| error.to_string())
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
                        JObject::from(env.new_string(key).map_err(|error| error.to_string())?);
                    env.call_method(
                        activity,
                        "deleteAokieSecureValue",
                        "(Ljava/lang/String;)I",
                        &[JValue::Object(&key)],
                    )
                    .and_then(|value| value.i())
                    .map_err(|error| error.to_string())
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
                        .map_err(|error| error.to_string())?
                        .l()
                        .map_err(|error| error.to_string())?;
                    if value.is_null() {
                        return Ok(None);
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map(Some)
                        .map_err(|error| error.to_string())
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
                            .map_err(|error| error.to_string())?,
                    );
                    let code =
                        JObject::from(env.new_string(code).map_err(|error| error.to_string())?);
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
                    .map_err(|error| error.to_string())
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
                        JObject::from(env.new_string(call_id).map_err(|error| error.to_string())?);
                    let outcome =
                        JObject::from(env.new_string(outcome).map_err(|error| error.to_string())?);
                    let reason =
                        JObject::from(env.new_string(reason).map_err(|error| error.to_string())?);
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
                    .map_err(|error| error.to_string())
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
                    .map_err(|error| error.to_string());
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
                    .map_err(|error| error.to_string());
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
                        .map_err(|error| error.to_string())?
                        .l()
                        .map_err(|error| error.to_string())?;
                    if value.is_null() {
                        return Err("Android native bridge returned no value".into());
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map_err(|error| error.to_string())
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
                            .map_err(|error| error.to_string())?,
                    );
                    let value = env
                        .call_method(
                            activity,
                            method,
                            "(Ljava/lang/String;)Ljava/lang/String;",
                            &[JValue::Object(&argument)],
                        )
                        .map_err(|error| error.to_string())?
                        .l()
                        .map_err(|error| error.to_string())?;
                    if value.is_null() {
                        return Err("Android native bridge returned no value".into());
                    }
                    env.get_string(&JString::from(value))
                        .map(String::from)
                        .map_err(|error| error.to_string())
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
