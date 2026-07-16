//! Canonical wire models for Aokie Companion realtime control.
//!
//! This crate deliberately contains no transport, authentication, WebRTC or
//! Bluetooth code. FormLogic decides who may participate, the realtime
//! gateway coordinates claims and leases, and the Aokie Desktop endpoint
//! remains the final authority for audio that can reach the cellular caller.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

pub mod v2;

pub const SCHEMA_VERSION: u16 = 1;
pub const MAX_JSON_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 200;
pub const MAX_PARTICIPANTS: usize = 6;
pub const MAX_CAPTIONS: usize = 200;

const MAX_TIMESTAMP_CHARS: usize = 64;
const MAX_DISPLAY_NAME_CHARS: usize = 120;
const MAX_CALLER_LABEL_CHARS: usize = 200;
const MAX_MASKED_NUMBER_CHARS: usize = 40;
const MAX_ROLE_CHARS: usize = 40;
const MAX_MODE_CHARS: usize = 40;
const MAX_SPEAKER_CHARS: usize = 40;
const MAX_CAPTION_TEXT_CHARS: usize = 2_000;
const MAX_ANSWER_CHARS: usize = 2_000;
const MAX_REASON_CHARS: usize = 200;
const MAX_ERROR_MESSAGE_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelephonyState {
    Ringing,
    Active,
    Held,
    Ending,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceMode {
    AokieActive,
    SoftHold,
    ConsultPending,
    ConsultActive,
    HumanPending,
    HumanActive,
    ReturningToAokie,
    Recovering,
    Unreachable,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaState {
    None,
    Ready,
    Receiving,
    Connecting,
    Active,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TalkOwner {
    Aokie,
    Hold,
    User {
        #[serde(rename = "userId")]
        user_id: String,
        #[serde(rename = "deviceId")]
        device_id: String,
        #[serde(rename = "leaseId")]
        lease_id: String,
        fence: u64,
    },
    None,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitTalkOwnerWire {
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UserTalkOwnerWire {
    kind: String,
    user_id: String,
    device_id: String,
    lease_id: String,
    fence: u64,
}

impl<'de> Deserialize<'de> for TalkOwner {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| serde::de::Error::custom("talk owner kind is missing"))?;
        match kind.as_str() {
            "aokie" | "hold" | "none" => {
                let wire: UnitTalkOwnerWire =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                match wire.kind.as_str() {
                    "aokie" => Ok(Self::Aokie),
                    "hold" => Ok(Self::Hold),
                    "none" => Ok(Self::None),
                    _ => Err(serde::de::Error::custom("unknown talk owner kind")),
                }
            }
            "user" => {
                let wire: UserTalkOwnerWire =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                if wire.kind != "user" {
                    return Err(serde::de::Error::custom("unknown talk owner kind"));
                }
                Ok(Self::User {
                    user_id: wire.user_id,
                    device_id: wire.device_id,
                    lease_id: wire.lease_id,
                    fence: wire.fence,
                })
            }
            _ => Err(serde::de::Error::custom("unknown talk owner kind")),
        }
    }
}

impl TalkOwner {
    fn validate(&self) -> Result<(), ProtocolError> {
        if let Self::User {
            user_id,
            device_id,
            lease_id,
            fence,
        } = self
        {
            require_identifier("talkOwner.userId", user_id)?;
            require_identifier("talkOwner.deviceId", device_id)?;
            require_identifier("talkOwner.leaseId", lease_id)?;
            require_safe_integer("talkOwner.fence", *fence, 1)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteCapabilities {
    pub live_captions: bool,
    pub monitor_audio: bool,
    pub software_hold: bool,
    pub voice_consult: bool,
    pub takeover: bool,
    pub end_caller: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisclosureState {
    pub required: bool,
    pub verified: bool,
    pub policy_version: String,
}

impl DisclosureState {
    fn validate(&self) -> Result<(), ProtocolError> {
        require_bounded_identifier("disclosure.policyVersion", &self.policy_version, 100)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallerSummary {
    pub label: Option<String>,
    pub masked_number: Option<String>,
}

impl CallerSummary {
    fn validate(&self) -> Result<(), ProtocolError> {
        if let Some(label) = &self.label {
            require_text("caller.label", label, MAX_CALLER_LABEL_CHARS)?;
        }
        if let Some(masked_number) = &self.masked_number {
            require_text(
                "caller.maskedNumber",
                masked_number,
                MAX_MASKED_NUMBER_CHARS,
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Participant {
    pub user_id: String,
    pub device_id: String,
    pub display_name: String,
    pub role: String,
    pub mode: String,
}

impl Participant {
    fn validate(&self) -> Result<(), ProtocolError> {
        require_identifier("participants.userId", &self.user_id)?;
        require_identifier("participants.deviceId", &self.device_id)?;
        require_text(
            "participants.displayName",
            &self.display_name,
            MAX_DISPLAY_NAME_CHARS,
        )?;
        require_bounded_identifier("participants.role", &self.role, MAX_ROLE_CHARS)?;
        require_bounded_identifier("participants.mode", &self.mode, MAX_MODE_CHARS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Caption {
    pub caption_id: String,
    pub speaker: String,
    pub text: String,
    pub occurred_at: String,
    pub final_text: bool,
}

impl Caption {
    fn validate(&self) -> Result<(), ProtocolError> {
        require_identifier("captions.captionId", &self.caption_id)?;
        require_bounded_identifier("captions.speaker", &self.speaker, MAX_SPEAKER_CHARS)?;
        require_text("captions.text", &self.text, MAX_CAPTION_TEXT_CHARS)?;
        require_timestamp("captions.occurredAt", &self.occurred_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TakeoverOffer {
    pub offer_id: String,
    pub expires_at: String,
}

impl TakeoverOffer {
    fn validate(&self) -> Result<(), ProtocolError> {
        require_identifier("takeoverOffer.offerId", &self.offer_id)?;
        require_timestamp("takeoverOffer.expiresAt", &self.expires_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssistanceRequest {
    pub request_id: String,
    pub expires_at: String,
}

impl AssistanceRequest {
    fn validate(&self) -> Result<(), ProtocolError> {
        require_identifier("assistanceRequest.requestId", &self.request_id)?;
        require_timestamp("assistanceRequest.expiresAt", &self.expires_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndConfirmation {
    pub confirmation_id: String,
    pub expires_at: String,
}

impl EndConfirmation {
    fn validate(&self) -> Result<(), ProtocolError> {
        require_identifier("endConfirmation.confirmationId", &self.confirmation_id)?;
        require_timestamp("endConfirmation.expiresAt", &self.expires_at)
    }
}

/// Authenticated gateway proof that the authoritative realtime state is idle.
///
/// This is a transport-level frame rather than a call snapshot. The WSS
/// admission binds the peer identity; `app_id` prevents an authenticated frame
/// from being accepted in a different app's client session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyncReadyFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub stream_nonce: String,
    pub sequence: u64,
}

impl SyncReadyFrame {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.kind != "sync_ready" {
            return Err(ProtocolError::InvalidField("kind"));
        }
        require_schema(self.schema_version)?;
        require_identifier("appId", &self.app_id)?;
        require_identifier("streamNonce", &self.stream_nonce)?;
        require_safe_integer("sequence", self.sequence, 0)
    }
}

/// Authoritative low-rate call and router state.
///
/// Participant presence is gateway-coordinated; audio ownership and the
/// epochs/revisions are acknowledged endpoint truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallSnapshot {
    pub schema_version: u16,
    pub app_id: String,
    pub call_id: String,
    /// Gateway/lane restart fence. `sequence` is monotonic only within this nonce.
    pub stream_nonce: String,
    pub sequence: u64,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub telephony_state: TelephonyState,
    pub service_mode: ServiceMode,
    pub talk_owner: TalkOwner,
    pub media_state: MediaState,
    pub gateway_reachable: bool,
    pub capabilities: RemoteCapabilities,
    pub disclosure: DisclosureState,
    pub secondary_call_policy: String,
    pub caller: Option<CallerSummary>,
    #[serde(default)]
    pub participants: Vec<Participant>,
    #[serde(default)]
    pub captions: Vec<Caption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_offer: Option<TakeoverOffer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistance_request: Option<AssistanceRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_confirmation: Option<EndConfirmation>,
    pub occurred_at: String,
}

impl CallSnapshot {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        require_schema(self.schema_version)?;
        require_identifier("appId", &self.app_id)?;
        require_identifier("callId", &self.call_id)?;
        require_identifier("streamNonce", &self.stream_nonce)?;
        require_safe_integer("sequence", self.sequence, 1)?;
        require_safe_integer("callEpoch", self.call_epoch, 0)?;
        require_safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        require_safe_integer("switchboardRevision", self.switchboard_revision, 0)?;
        require_safe_integer("remoteRevision", self.remote_revision, 0)?;
        self.talk_owner.validate()?;
        self.disclosure.validate()?;
        if self.secondary_call_policy != "miss_and_callback" {
            return Err(ProtocolError::InvalidField("secondaryCallPolicy"));
        }
        if let Some(caller) = &self.caller {
            caller.validate()?;
        }
        if self.participants.len() > MAX_PARTICIPANTS {
            return Err(ProtocolError::InvalidField("participants"));
        }
        for participant in &self.participants {
            participant.validate()?;
        }
        if self.captions.len() > MAX_CAPTIONS {
            return Err(ProtocolError::InvalidField("captions"));
        }
        if !self.capabilities.live_captions && !self.captions.is_empty() {
            return Err(ProtocolError::InvalidField("captions"));
        }
        for caption in &self.captions {
            caption.validate()?;
        }
        if let Some(offer) = &self.takeover_offer {
            offer.validate()?;
        }
        if let Some(request) = &self.assistance_request {
            request.validate()?;
        }
        if let Some(confirmation) = &self.end_confirmation {
            confirmation.validate()?;
        }
        require_timestamp("occurredAt", &self.occurred_at)?;
        self.validate_mode_matrix()
    }

    fn validate_mode_matrix(&self) -> Result<(), ProtocolError> {
        let owner_matches = match self.service_mode {
            ServiceMode::AokieActive => matches!(self.talk_owner, TalkOwner::Aokie),
            ServiceMode::SoftHold
            | ServiceMode::ConsultPending
            | ServiceMode::ConsultActive
            | ServiceMode::HumanPending
            | ServiceMode::ReturningToAokie
            | ServiceMode::Recovering
            | ServiceMode::Unreachable => matches!(self.talk_owner, TalkOwner::Hold),
            ServiceMode::HumanActive => matches!(self.talk_owner, TalkOwner::User { .. }),
            ServiceMode::Ended => matches!(self.talk_owner, TalkOwner::None),
        };
        if !owner_matches {
            return Err(ProtocolError::UnsafeState(
                "service mode and talk owner do not match",
            ));
        }
        if matches!(self.service_mode, ServiceMode::HumanActive)
            && !matches!(self.media_state, MediaState::Active)
        {
            return Err(ProtocolError::UnsafeState(
                "human_active requires confirmed active media",
            ));
        }

        let service_ended = matches!(self.service_mode, ServiceMode::Ended);
        let telephony_ended = matches!(self.telephony_state, TelephonyState::Ended);
        if service_ended != telephony_ended {
            return Err(ProtocolError::UnsafeState(
                "service and telephony ended states must agree",
            ));
        }
        if service_ended && !matches!(self.media_state, MediaState::None) {
            return Err(ProtocolError::UnsafeState(
                "ended calls must have no active media",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandType {
    MonitorStart,
    MonitorStop,
    AssistanceRespond,
    ConsultClaim,
    ConsultEnd,
    TakeoverClaim,
    ResumeAokie,
    EndCaller,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandEnvelope {
    pub schema_version: u16,
    pub command_id: String,
    pub idempotency_key: String,
    pub call_id: String,
    pub expected_switchboard_revision: u64,
    pub expected_remote_revision: u64,
    pub expected_call_epoch: u64,
    pub expected_owner_epoch: u64,
    #[serde(rename = "type")]
    pub command_type: CommandType,
    #[serde(default)]
    pub payload: Value,
}

impl CommandEnvelope {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        require_schema(self.schema_version)?;
        require_identifier("commandId", &self.command_id)?;
        require_identifier("callId", &self.call_id)?;
        require_idempotency_key(&self.idempotency_key)?;
        require_safe_integer(
            "expectedSwitchboardRevision",
            self.expected_switchboard_revision,
            0,
        )?;
        require_safe_integer("expectedRemoteRevision", self.expected_remote_revision, 0)?;
        require_safe_integer("expectedCallEpoch", self.expected_call_epoch, 0)?;
        require_safe_integer("expectedOwnerEpoch", self.expected_owner_epoch, 0)?;

        match self.command_type {
            CommandType::MonitorStart | CommandType::MonitorStop => {
                parse_payload::<EmptyPayload>(&self.payload)?;
            }
            CommandType::ConsultClaim | CommandType::TakeoverClaim => {
                let payload = parse_payload::<OfferClaimPayload>(&self.payload)?;
                require_identifier("payload.offerId", &payload.offer_id)?;
            }
            CommandType::ConsultEnd => {
                let payload = parse_payload::<ConsultEndPayload>(&self.payload)?;
                require_identifier("payload.leaseId", &payload.lease_id)?;
            }
            CommandType::ResumeAokie => {
                let payload = parse_payload::<ResumeAokiePayload>(&self.payload)?;
                require_identifier("payload.leaseId", &payload.lease_id)?;
                require_text("payload.reason", &payload.reason, MAX_REASON_CHARS)?;
            }
            CommandType::EndCaller => {
                let payload = parse_payload::<EndCallerPayload>(&self.payload)?;
                require_identifier("payload.confirmationId", &payload.confirmation_id)?;
            }
            CommandType::AssistanceRespond => {
                let payload = parse_payload::<AssistanceRespondPayload>(&self.payload)?;
                require_identifier("payload.requestId", &payload.request_id)?;
                require_identifier("payload.answerId", &payload.answer_id)?;
                require_text("payload.answer", &payload.answer, MAX_ANSWER_CHARS)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyPayload {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OfferClaimPayload {
    offer_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConsultEndPayload {
    lease_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResumeAokiePayload {
    lease_id: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EndCallerPayload {
    confirmation_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssistanceRespondPayload {
    request_id: String,
    answer_id: String,
    answer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    InvalidMessage,
    Unauthorized,
    Forbidden,
    StaleCall,
    StaleCallEpoch,
    StaleOwnerEpoch,
    StaleRemoteRevision,
    StaleSwitchboardRevision,
    OfferExpired,
    ClaimLost,
    LeaseExpired,
    MediaUnavailable,
    EndpointUnreachable,
    CommandFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandError {
    pub code: ProtocolErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandAck {
    pub command_id: String,
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CommandError>,
}

impl CommandAck {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        require_identifier("commandId", &self.command_id)?;
        match (self.accepted, &self.error) {
            (true, None) => Ok(()),
            (false, Some(error)) => {
                require_text("error.message", &error.message, MAX_ERROR_MESSAGE_CHARS)
            }
            (true, Some(_)) => Err(ProtocolError::InvalidField("error")),
            (false, None) => Err(ProtocolError::InvalidField("error")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    UnsupportedSchema(u16),
    InvalidField(&'static str),
    UnsafeState(&'static str),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema(version) => write!(f, "unsupported schema version {version}"),
            Self::InvalidField(field) => write!(f, "invalid protocol field {field}"),
            Self::UnsafeState(reason) => write!(f, "unsafe protocol state: {reason}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

fn require_schema(version: u16) -> Result<(), ProtocolError> {
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedSchema(version))
    }
}

fn require_safe_integer(
    field: &'static str,
    value: u64,
    minimum: u64,
) -> Result<(), ProtocolError> {
    if (minimum..=MAX_JSON_SAFE_INTEGER).contains(&value) {
        Ok(())
    } else {
        Err(ProtocolError::InvalidField(field))
    }
}

fn require_identifier(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    require_bounded_identifier(field, value, 200)
}

fn require_bounded_identifier(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), ProtocolError> {
    if !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(ProtocolError::InvalidField(field))
    }
}

fn require_idempotency_key(value: &str) -> Result<(), ProtocolError> {
    if !value.is_empty()
        && value.len() <= MAX_IDEMPOTENCY_KEY_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(ProtocolError::InvalidField("idempotencyKey"))
    }
}

fn require_text(
    field: &'static str,
    value: &str,
    maximum_chars: usize,
) -> Result<(), ProtocolError> {
    let length = value.chars().count();
    let has_disallowed_control = value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'));
    if length > 0 && length <= maximum_chars && !value.trim().is_empty() && !has_disallowed_control
    {
        Ok(())
    } else {
        Err(ProtocolError::InvalidField(field))
    }
}

fn require_timestamp(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    require_text(field, value, MAX_TIMESTAMP_CHARS)?;
    if !value.is_ascii() || value.as_bytes().get(10) != Some(&b'T') {
        return Err(ProtocolError::InvalidField(field));
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|_| ())
        .map_err(|_| ProtocolError::InvalidField(field))
}

fn parse_payload<T: DeserializeOwned>(payload: &Value) -> Result<T, ProtocolError> {
    if !payload.is_object() {
        return Err(ProtocolError::InvalidField("payload"));
    }
    serde_json::from_value(payload.clone()).map_err(|_| ProtocolError::InvalidField("payload"))
}

#[cfg(test)]
mod tests {
    use super::{
        CallSnapshot, CommandAck, CommandEnvelope, MediaState, ProtocolError, ServiceMode,
        TalkOwner, TelephonyState, MAX_CAPTIONS, MAX_DISPLAY_NAME_CHARS, MAX_JSON_SAFE_INTEGER,
        MAX_PARTICIPANTS,
    };
    use serde_json::{json, Value};

    fn snapshot() -> CallSnapshot {
        let raw =
            include_str!("../../../docs/contracts/fixtures/aokie-companion-live-snapshot.v1.json");
        serde_json::from_str(raw).expect("fixture parses")
    }

    fn command_value(command_type: &str, payload: Value) -> Value {
        json!({
            "schemaVersion": 1,
            "commandId": "cmd_test",
            "idempotencyKey": "mobile:device_test:cmd_test",
            "callId": "call_test",
            "expectedSwitchboardRevision": 8,
            "expectedRemoteRevision": 19,
            "expectedCallEpoch": 7,
            "expectedOwnerEpoch": 12,
            "type": command_type,
            "payload": payload
        })
    }

    fn command(command_type: &str, payload: Value) -> CommandEnvelope {
        serde_json::from_value(command_value(command_type, payload)).expect("command parses")
    }

    #[test]
    fn canonical_snapshot_fixture_round_trips_and_is_safe() {
        let snapshot = snapshot();
        snapshot.validate().expect("fixture validates");
        assert_eq!(snapshot.stream_nonce, "stream_01JZQ1K5M8P4R2V7X9C6B3N0DT");
        let encoded = serde_json::to_string(&snapshot).expect("snapshot serializes");
        let decoded: CallSnapshot = serde_json::from_str(&encoded).expect("round trip parses");
        assert_eq!(snapshot, decoded);
    }

    #[test]
    fn stream_nonce_is_required_and_must_be_a_safe_restart_fence() {
        let mut value = serde_json::to_value(snapshot()).expect("snapshot encodes");
        value
            .as_object_mut()
            .expect("snapshot object")
            .remove("streamNonce");
        assert!(serde_json::from_value::<CallSnapshot>(value).is_err());

        for invalid in ["", "lane restart", "nonce/with/slashes"] {
            let mut value = snapshot();
            value.stream_nonce = invalid.into();
            assert!(matches!(
                value.validate(),
                Err(ProtocolError::InvalidField("streamNonce"))
            ));
        }
    }

    #[test]
    fn timestamps_require_real_rfc3339_calendar_dates() {
        for invalid in [
            "2026-02-30T06:00:05Z",
            "2025-02-29T06:00:05Z",
            "2026-13-01T06:00:05Z",
            "2026-07-15T25:00:05Z",
            "2026-07-15 06:00:05Z",
        ] {
            let mut value = snapshot();
            value.occurred_at = invalid.into();
            assert!(
                matches!(
                    value.validate(),
                    Err(ProtocolError::InvalidField("occurredAt"))
                ),
                "timestamp should be rejected: {invalid}"
            );
        }

        let mut value = snapshot();
        value.occurred_at = "2024-02-29T06:00:05+11:00".into();
        value.validate().expect("leap day with offset is RFC3339");
    }

    #[test]
    fn canonical_command_fixture_round_trips_and_is_idempotent() {
        let raw = include_str!(
            "../../../docs/contracts/fixtures/aokie-companion-takeover-command.v1.json"
        );
        let command: CommandEnvelope = serde_json::from_str(raw).expect("fixture parses");
        command.validate().expect("fixture validates");
        assert_eq!(
            command.idempotency_key,
            "mobile:device_iphone15:cmd_takeover_01"
        );
    }

    #[test]
    fn every_wire_counter_is_bounded_to_json_safe_integer_range() {
        let counter_fields = [
            "sequence",
            "callEpoch",
            "ownerEpoch",
            "switchboardRevision",
            "remoteRevision",
        ];
        for field in counter_fields {
            let mut value = serde_json::to_value(snapshot()).expect("snapshot encodes");
            value[field] = json!(MAX_JSON_SAFE_INTEGER + 1);
            let parsed: CallSnapshot = serde_json::from_value(value).expect("u64 parses");
            assert!(matches!(
                parsed.validate(),
                Err(ProtocolError::InvalidField(found)) if found == field
            ));
        }

        let expected_fields = [
            "expectedSwitchboardRevision",
            "expectedRemoteRevision",
            "expectedCallEpoch",
            "expectedOwnerEpoch",
        ];
        for field in expected_fields {
            let mut value = command_value("monitor_start", json!({}));
            value[field] = json!(MAX_JSON_SAFE_INTEGER + 1);
            let parsed: CommandEnvelope = serde_json::from_value(value).expect("u64 parses");
            assert!(parsed.validate().is_err(), "{field} must be bounded");
        }

        let mut snapshot = snapshot();
        snapshot.service_mode = ServiceMode::HumanActive;
        snapshot.talk_owner = TalkOwner::User {
            user_id: "user_test".into(),
            device_id: "device_test".into(),
            lease_id: "lease_test".into(),
            fence: MAX_JSON_SAFE_INTEGER + 1,
        };
        assert!(matches!(
            snapshot.validate(),
            Err(ProtocolError::InvalidField("talkOwner.fence"))
        ));
    }

    #[test]
    fn service_modes_enforce_the_single_safe_talk_owner_matrix() {
        let cases = [
            (ServiceMode::AokieActive, TalkOwner::Aokie, true),
            (ServiceMode::AokieActive, TalkOwner::Hold, false),
            (ServiceMode::SoftHold, TalkOwner::Hold, true),
            (ServiceMode::ConsultPending, TalkOwner::Hold, true),
            (ServiceMode::ConsultActive, TalkOwner::Hold, true),
            (ServiceMode::HumanPending, TalkOwner::Hold, true),
            (ServiceMode::ReturningToAokie, TalkOwner::Hold, true),
            (ServiceMode::Recovering, TalkOwner::Hold, true),
            (ServiceMode::Unreachable, TalkOwner::Hold, true),
            (ServiceMode::HumanActive, TalkOwner::Aokie, false),
            (
                ServiceMode::HumanActive,
                TalkOwner::User {
                    user_id: "user_test".into(),
                    device_id: "device_test".into(),
                    lease_id: "lease_test".into(),
                    fence: 1,
                },
                true,
            ),
        ];

        for (mode, owner, expected) in cases {
            let mut snapshot = snapshot();
            snapshot.service_mode = mode;
            snapshot.talk_owner = owner;
            if matches!(snapshot.service_mode, ServiceMode::HumanActive)
                && matches!(snapshot.talk_owner, TalkOwner::User { .. })
            {
                snapshot.media_state = MediaState::Active;
            }
            assert_eq!(snapshot.validate().is_ok(), expected);
        }

        let mut unconfirmed_human = snapshot();
        unconfirmed_human.service_mode = ServiceMode::HumanActive;
        unconfirmed_human.talk_owner = TalkOwner::User {
            user_id: "user_test".into(),
            device_id: "device_test".into(),
            lease_id: "lease_test".into(),
            fence: 1,
        };
        assert!(matches!(
            unconfirmed_human.validate(),
            Err(ProtocolError::UnsafeState(_))
        ));
    }

    #[test]
    fn ended_state_is_atomic_and_rejects_every_contradiction() {
        let mut ended = snapshot();
        ended.service_mode = ServiceMode::Ended;
        ended.telephony_state = TelephonyState::Ended;
        ended.media_state = MediaState::None;
        ended.talk_owner = TalkOwner::None;
        ended.validate().expect("fully ended state is safe");

        for contradiction in 0..3 {
            let mut value = ended.clone();
            match contradiction {
                0 => value.telephony_state = TelephonyState::Active,
                1 => value.media_state = MediaState::Ready,
                _ => value.talk_owner = TalkOwner::Hold,
            }
            assert!(matches!(
                value.validate(),
                Err(ProtocolError::UnsafeState(_))
            ));
        }

        let mut value = snapshot();
        value.telephony_state = TelephonyState::Ended;
        assert!(matches!(
            value.validate(),
            Err(ProtocolError::UnsafeState(_))
        ));
    }

    #[test]
    fn lease_bound_user_owner_requires_safe_ids_and_positive_fence() {
        for (user_id, device_id, lease_id, fence) in [
            ("", "device_ok", "lease_ok", 1),
            ("user ok", "device_ok", "lease_ok", 1),
            ("user_ok", "", "lease_ok", 1),
            ("user_ok", "device_ok", "", 1),
            ("user_ok", "device_ok", "lease_ok", 0),
        ] {
            let mut value = snapshot();
            value.service_mode = ServiceMode::HumanActive;
            value.talk_owner = TalkOwner::User {
                user_id: user_id.into(),
                device_id: device_id.into(),
                lease_id: lease_id.into(),
                fence,
            };
            assert!(value.validate().is_err());
        }
    }

    #[test]
    fn snapshot_collections_policy_and_strings_are_bounded() {
        let mut value = snapshot();
        value.secondary_call_policy = "queue_and_answer".into();
        assert!(matches!(
            value.validate(),
            Err(ProtocolError::InvalidField("secondaryCallPolicy"))
        ));

        let mut value = snapshot();
        value.participants = vec![value.participants[0].clone(); MAX_PARTICIPANTS + 1];
        assert!(matches!(
            value.validate(),
            Err(ProtocolError::InvalidField("participants"))
        ));

        let mut value = snapshot();
        value.captions = vec![value.captions[0].clone(); MAX_CAPTIONS + 1];
        assert!(matches!(
            value.validate(),
            Err(ProtocolError::InvalidField("captions"))
        ));

        let mut value = snapshot();
        value.capabilities.live_captions = false;
        assert!(matches!(
            value.validate(),
            Err(ProtocolError::InvalidField("captions"))
        ));
        value.captions.clear();
        value
            .validate()
            .expect("caption-disabled snapshots are valid only without captions");

        let mut value = snapshot();
        value.captions[0].text = " \n\t".into();
        assert!(matches!(
            value.validate(),
            Err(ProtocolError::InvalidField("captions.text"))
        ));

        let mut value = snapshot();
        value.participants[0].display_name = "x".repeat(MAX_DISPLAY_NAME_CHARS + 1);
        assert!(value.validate().is_err());
    }

    #[test]
    fn opportunity_objects_are_bounded_and_unknown_fields_are_denied() {
        let mut value = serde_json::to_value(snapshot()).expect("snapshot encodes");
        value["takeoverOffer"]["offerId"] = json!("");
        let parsed: CallSnapshot = serde_json::from_value(value).expect("shape parses");
        assert!(matches!(
            parsed.validate(),
            Err(ProtocolError::InvalidField("takeoverOffer.offerId"))
        ));

        let mut value = serde_json::to_value(snapshot()).expect("snapshot encodes");
        value["assistanceRequest"]["expiresAt"] = json!("soon");
        let parsed: CallSnapshot = serde_json::from_value(value).expect("shape parses");
        assert!(matches!(
            parsed.validate(),
            Err(ProtocolError::InvalidField("assistanceRequest.expiresAt"))
        ));

        let mut value = serde_json::to_value(snapshot()).expect("snapshot encodes");
        value["participants"][0]["unexpected"] = json!(true);
        assert!(serde_json::from_value::<CallSnapshot>(value).is_err());

        let mut value = serde_json::to_value(snapshot()).expect("snapshot encodes");
        value["talkOwner"]["unexpected"] = json!(true);
        assert!(serde_json::from_value::<CallSnapshot>(value).is_err());
    }

    #[test]
    fn command_payloads_are_discriminated_and_deny_extra_fields() {
        let valid = [
            ("monitor_start", json!({})),
            ("monitor_stop", json!({})),
            ("takeover_claim", json!({ "offerId": "offer_test" })),
            ("consult_claim", json!({ "offerId": "offer_test" })),
            ("consult_end", json!({ "leaseId": "lease_test" })),
            (
                "resume_aokie",
                json!({ "leaseId": "lease_test", "reason": "operator_return" }),
            ),
            (
                "end_caller",
                json!({ "confirmationId": "confirmation_test" }),
            ),
            (
                "assistance_respond",
                json!({
                    "requestId": "request_test",
                    "answerId": "answer_test",
                    "answer": "The keys can go in the secure box."
                }),
            ),
        ];
        for (command_type, payload) in valid {
            command(command_type, payload)
                .validate()
                .unwrap_or_else(|error| panic!("{command_type} should validate: {error}"));
        }

        let invalid = [
            ("monitor_start", json!({ "offerId": "offer_test" })),
            ("takeover_claim", json!({})),
            ("consult_claim", json!({ "offerId": "bad id" })),
            ("consult_end", json!({ "offerId": "offer_test" })),
            ("resume_aokie", json!({ "leaseId": "lease_test" })),
            ("end_caller", json!({ "confirmationId": "" })),
            (
                "assistance_respond",
                json!({
                    "requestId": "request_test",
                    "answerId": "answer_test",
                    "answer": "  "
                }),
            ),
            (
                "assistance_respond",
                json!({
                    "requestId": "request_test",
                    "answerId": "answer_test",
                    "answer": "ok",
                    "unexpected": true
                }),
            ),
        ];
        for (command_type, payload) in invalid {
            assert!(
                command(command_type, payload).validate().is_err(),
                "{command_type} must reject an invalid payload"
            );
        }
    }

    #[test]
    fn command_ack_is_typed_and_error_presence_matches_acceptance() {
        let accepted: CommandAck = serde_json::from_value(json!({
            "commandId": "cmd_test",
            "accepted": true
        }))
        .expect("accepted ack parses");
        accepted.validate().expect("accepted ack validates");

        let rejected: CommandAck = serde_json::from_value(json!({
            "commandId": "cmd_test",
            "accepted": false,
            "error": {
                "code": "stale_owner_epoch",
                "message": "The talk owner changed before the claim was committed."
            }
        }))
        .expect("rejected ack parses");
        rejected.validate().expect("rejected ack validates");

        let contradictory: CommandAck = serde_json::from_value(json!({
            "commandId": "cmd_test",
            "accepted": true,
            "error": { "code": "command_failed", "message": "failed" }
        }))
        .expect("contradictory ack parses");
        assert!(contradictory.validate().is_err());

        assert!(serde_json::from_value::<CommandAck>(json!({
            "commandId": "cmd_test",
            "accepted": true,
            "unexpected": true
        }))
        .is_err());
    }
}
