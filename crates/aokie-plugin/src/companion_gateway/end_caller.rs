//! End-caller result encoding and gateway frame parsing helpers.

#[allow(unused_imports)]
use super::*;

pub(super) fn end_caller_result(
    execute: &PluginEndCallerExecuteFrame,
    outcome: EndCallerOutcome,
    failure: Option<(&str, &str)>,
) -> PluginEndCallerResultFrame {
    PluginEndCallerResultFrame {
        kind: "end_caller_result".into(),
        schema_version: SCHEMA_VERSION,
        app_id: execute.app_id.clone(),
        operation_id: execute.operation_id.clone(),
        confirmation_id: execute.confirmation_id.clone(),
        device_id: execute.device_id.clone(),
        call_id: execute.call_id.clone(),
        call_epoch: execute.call_epoch,
        owner_epoch: execute.owner_epoch,
        switchboard_revision: execute.switchboard_revision,
        remote_revision: execute.remote_revision,
        lease_id: execute.lease_id.clone(),
        fence: execute.fence,
        outcome,
        code: failure.map(|(code, _)| code.to_owned()),
        message: failure.map(|(_, message)| message.to_owned()),
    }
}

pub(super) fn encode_end_caller_failure(
    execute: &PluginEndCallerExecuteFrame,
    code: &str,
    message: &str,
) -> Result<String, WorkerError> {
    let frame = end_caller_result(execute, EndCallerOutcome::Failed, Some((code, message)));
    frame
        .validate()
        .map_err(|_| WorkerError::reconnect("Companion caller-ending failure is invalid"))?;
    serde_json::to_string(&frame)
        .map_err(|_| WorkerError::reconnect("Companion caller-ending failure could not be encoded"))
}

pub(super) fn parse_gateway_frame<T>(encoded: &str) -> Result<T, WorkerError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(encoded)
        .map_err(|_| WorkerError::reconnect("Companion gateway frame has an invalid shape"))
}

pub(super) fn sanitize_gateway_code(code: &str) -> String {
    let safe = code
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .take(80)
        .collect::<String>();
    if safe.is_empty() {
        "an error".into()
    } else {
        safe
    }
}

pub(super) fn lease_ttl_ms(claims: &LeaseClaims) -> Result<u64, WorkerError> {
    let remaining = claims.expires_at.saturating_sub(unix_now()?);
    if remaining == 0 || remaining > 300 {
        return Err(WorkerError::expired("Companion media lease is expired"));
    }
    Ok(remaining.saturating_mul(1_000))
}

pub(super) fn binding_for_claims(claims: &LeaseClaims) -> SessionBinding {
    let mode = match (claims.mode, claims.phase) {
        (LeaseMode::Monitor, _) => MediaMode::Monitor,
        (LeaseMode::Consult, LeasePhase::Prepared) => MediaMode::PreparedConsult,
        (LeaseMode::Consult, LeasePhase::Active) => MediaMode::Consult,
        (LeaseMode::Takeover, LeasePhase::Prepared) => MediaMode::PreparedTalk,
        (LeaseMode::Takeover, LeasePhase::Active) => MediaMode::Talk,
    };
    SessionBinding {
        rtc_session_id: claims.rtc_session_id.clone(),
        call_id: claims.call_id.clone(),
        call_epoch: claims.call_epoch,
        owner_epoch: claims.owner_epoch,
        device_id: claims.device_id.clone(),
        mode,
        lease_id: Some(claims.lease_id.clone()),
        fence: claims.fence,
    }
}
