//! Typed, redacted mobile-management API.
//!
//! The OAuth bearer stays in `ManagedAuthState`. These commands expose only
//! bounded domain records and reject unknown response fields.

use std::collections::HashSet;

use chrono::NaiveDateTime;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::managed_auth::ManagedAuthState;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_COMPANION_CAPABILITIES: usize = 14;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
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

/// An enum value as a newer server may send it.
///
/// A name this build knows becomes `Known`; any other well-formed name is kept
/// as `Unknown` so the record holding it can be dropped from its list (see
/// `ValidateResponse::sanitize`) instead of failing the whole response, which
/// used to take the screen down on every phone that had not updated yet. No
/// `Unknown` value ever reaches the WebView.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tolerant<T> {
    Known(T),
    Unknown(String),
}

impl<T> Tolerant<T> {
    fn is_known(&self) -> bool {
        matches!(self, Self::Known(_))
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Tolerant<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::IntoDeserializer;
        let name = String::deserialize(deserializer)?;
        let known: Result<T, serde::de::value::Error> =
            T::deserialize(name.as_str().into_deserializer());
        match known {
            Ok(value) => Ok(Self::Known(value)),
            Err(_) if validate_id(&name, "enum value").is_ok() && name.len() <= 64 => {
                Ok(Self::Unknown(name))
            }
            Err(_) => Err(serde::de::Error::custom("enum value is malformed")),
        }
    }
}

impl<T: Serialize> Serialize for Tolerant<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Known(value) => value.serialize(serializer),
            Self::Unknown(name) => name.serialize(serializer),
        }
    }
}

/// A capability list with the names this build does not know dropped: a
/// newer server's capability is one this build cannot use anyway.
fn deserialize_known_capabilities<'de, D>(
    deserializer: D,
) -> Result<Vec<CompanionCapability>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let names = Vec::<Tolerant<CompanionCapability>>::deserialize(deserializer)?;
    if names.len() > 64 {
        return Err(serde::de::Error::custom("capability list is too long"));
    }
    Ok(names
        .into_iter()
        .filter_map(|name| match name {
            Tolerant::Known(capability) => Some(capability),
            Tolerant::Unknown(_) => None,
        })
        .collect())
}

/// Fold display text into one line within its byte limit (see
/// `aokie_protocol::text`): one multi-line summary used to fail the whole
/// Calls list, and the WebView refuses control characters too.
fn display_line(value: &mut String, maximum: usize) {
    *value = aokie_protocol::text::sanitize_line(value, maximum);
}

fn optional_display_line(value: &mut RequiredNullable<String>, maximum: usize) {
    if let Some(text) = value.0.as_mut() {
        display_line(text, maximum);
        if text.is_empty() {
            value.0 = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionCapability {
    StateRead,
    CallerRead,
    CaptionsRead,
    ParticipantsRead,
    ParticipantIdentityRead,
    AudioLevelsRead,
    Monitor,
    Consult,
    Takeover,
    ResumeAokie,
    RtcSignal,
    AssistanceRead,
    AssistanceRespond,
    EndCaller,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompanionAvailabilityStatus {
    Available,
    Busy,
    Offline,
    DoNotDisturb,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RoutingPolicy {
    All,
    Priority,
    RoundRobin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionMode {
    Monitor,
    Consult,
    Takeover,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionState {
    Prepared,
    Joined,
    Left,
    Revoked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActivityEventType {
    AdmissionIssued,
    MonitorJoined,
    MonitorLeft,
    ConsultJoined,
    ConsultLeft,
    TakeoverPrepared,
    TakeoverJoined,
    TakeoverLeft,
    ReturnedToAokie,
    SessionRecovered,
    SessionRevoked,
    EndpointRevoked,
    CallAlertTargeted,
    AssistanceTargeted,
    TakeoverTargeted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompanionMembership {
    app_id: String,
    app_slug: String,
    status: ActiveStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActiveStatus {
    Active,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompanionDevice {
    id: String,
    user_id: String,
    app_id: String,
    subject_id: String,
    role: MobileRole,
    display_name: String,
    #[serde(deserialize_with = "deserialize_known_capabilities")]
    grants: Vec<CompanionCapability>,
    approved_at: String,
    last_seen_at: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    revoked_at: RequiredNullable<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MobileRole {
    Mobile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AvailabilityRecord {
    availability: CompanionAvailabilityStatus,
    updated_at: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    expires_at: RequiredNullable<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TeamMember {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    staff_id: RequiredNullable<String>,
    display_name: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    role_name: RequiredNullable<String>,
    is_current_user: bool,
    // Schema 1 omitted this non-authoritative UI projection. Accepting that
    // exact older shape keeps a newly updated client rollback-compatible;
    // native serialization still always emits the explicit boolean to TS.
    #[serde(default)]
    is_current_device: bool,
    priority: u32,
    enabled: bool,
    availability: CompanionAvailabilityStatus,
    availability_updated_at: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    availability_expires_at: RequiredNullable<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StaffMember {
    id: String,
    display_name: String,
    role_name: String,
    is_current_user: bool,
    is_owner: bool,
    companion_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RoutingGroup {
    id: String,
    name: String,
    policy: RoutingPolicy,
    enabled: bool,
    members: Vec<TeamMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompanionActivity {
    id: String,
    event_id: String,
    app_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    session_record_id: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    call_id: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    device_id: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    actor_user_id: RequiredNullable<String>,
    subject_id: String,
    event_type: Tolerant<ActivityEventType>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    mode: RequiredNullable<Tolerant<SessionMode>>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    reason: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    owner_epoch: RequiredNullable<u64>,
    occurred_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompanionSession {
    id: String,
    session_id: String,
    call_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    device_id: RequiredNullable<String>,
    subject_id: String,
    mode: Tolerant<SessionMode>,
    state: Tolerant<SessionState>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    joined_at: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    ended_at: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    end_reason: RequiredNullable<String>,
    last_event_id: String,
    last_event_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionHistory {
    activity: Vec<CompanionActivity>,
    sessions: Vec<CompanionSession>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CallDirection {
    Inbound,
    Outbound,
    // A direction a newer server names reads as unknown, which the WebView
    // already shows.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CallRecordAccess {
    Full,
    Own,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CallRecord {
    id: String,
    call_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    caller_name: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    masked_number: RequiredNullable<String>,
    status: String,
    direction: CallDirection,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    summary: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    started_at: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    ended_at: RequiredNullable<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    duration_seconds: RequiredNullable<u64>,
    follow_up_required: bool,
    submitted_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionCallRecords {
    records: Vec<CallRecord>,
    access: CallRecordAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CallTranscriptTurn {
    id: String,
    speaker: String,
    text: String,
    occurred_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CallFollowUp {
    id: String,
    summary: String,
    status: String,
    priority: String,
    submitted_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionCallRecordDetail {
    record: CallRecord,
    transcript: Vec<CallTranscriptTurn>,
    follow_ups: Vec<CallFollowUp>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PushKind {
    Fcm,
    Apns,
    ApnsVoip,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PushMode {
    Managed,
    Broker,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PushProvider {
    Fcm,
    Apns,
    Broker,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PushEnvironment {
    Sandbox,
    Production,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PushEndpoint {
    id: String,
    app_id: String,
    device_id: String,
    kind: Tolerant<PushKind>,
    mode: Tolerant<PushMode>,
    provider: Tolerant<PushProvider>,
    environment: Tolerant<PushEnvironment>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    topic: RequiredNullable<String>,
    fingerprint: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    invalidated_at: RequiredNullable<String>,
    rotated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionBootstrap {
    membership: CompanionMembership,
    device: CompanionDevice,
    #[serde(deserialize_with = "deserialize_known_capabilities")]
    capabilities: Vec<CompanionCapability>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    availability: RequiredNullable<AvailabilityRecord>,
    routing_groups: Vec<RoutingGroup>,
    staff: Vec<StaffMember>,
    history: CompanionHistory,
    push_endpoints: Vec<PushEndpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionRouting {
    routing_groups: Vec<RoutingGroup>,
    staff: Vec<StaffMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionAvailability {
    app_id: String,
    device_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    availability: RequiredNullable<AvailabilityRecord>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetAvailabilityRequest {
    availability: CompanionAvailabilityStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in_seconds: Option<u32>,
}

trait ValidateResponse {
    /// Make the response safe to keep before it is validated: fold display
    /// text into bounded single lines, and drop the list entries a newer
    /// server added values this build does not know to. Identifiers are never
    /// rewritten; they stay strict in `validate`.
    fn sanitize(&mut self) {}

    fn validate(&self) -> Result<(), String>;
}

#[tauri::command]
pub async fn companion_bootstrap(
    app: AppHandle,
    auth: State<'_, ManagedAuthState>,
) -> Result<CompanionBootstrap, String> {
    let response = auth
        .mobile_api_request(
            &app,
            Method::GET,
            "/api/aokie-companion/mobile/bootstrap",
            &[],
            None,
        )
        .await?;
    let parsed: CompanionBootstrap = parse_response(&response.body)?;
    if parsed.membership.app_id != response.app_id || parsed.device.subject_id != response.device_id
    {
        return Err("Companion bootstrap does not match the native managed session".into());
    }
    Ok(parsed)
}

#[tauri::command]
pub async fn companion_history(
    app: AppHandle,
    auth: State<'_, ManagedAuthState>,
    limit: Option<u16>,
    before: Option<u64>,
) -> Result<CompanionHistory, String> {
    if limit.is_some_and(|limit| !(1..=200).contains(&limit))
        || before.is_some_and(|before| before == 0 || before > MAX_SAFE_INTEGER)
    {
        return Err("Companion history pagination is invalid".into());
    }
    let mut query = Vec::with_capacity(2);
    if let Some(limit) = limit {
        query.push(("limit", limit.to_string()));
    }
    if let Some(before) = before {
        query.push(("before", before.to_string()));
    }
    let response = auth
        .mobile_api_request(
            &app,
            Method::GET,
            "/api/aokie-companion/mobile/history",
            &query,
            None,
        )
        .await?;
    let parsed: CompanionHistory = parse_response(&response.body)?;
    if parsed
        .activity
        .iter()
        .any(|activity| activity.app_id != response.app_id)
    {
        return Err("Companion history does not match the native managed app".into());
    }
    Ok(parsed)
}

#[tauri::command]
pub async fn companion_routing(
    app: AppHandle,
    auth: State<'_, ManagedAuthState>,
) -> Result<CompanionRouting, String> {
    let response = auth
        .mobile_api_request(
            &app,
            Method::GET,
            "/api/aokie-companion/mobile/routing",
            &[],
            None,
        )
        .await?;
    parse_response(&response.body)
}

#[tauri::command]
pub async fn companion_call_records(
    app: AppHandle,
    auth: State<'_, ManagedAuthState>,
    limit: Option<u16>,
) -> Result<CompanionCallRecords, String> {
    if limit.is_some_and(|limit| !(1..=100).contains(&limit)) {
        return Err("Companion call-record limit must be 1 to 100".into());
    }
    let query = limit
        .map(|limit| vec![("limit", limit.to_string())])
        .unwrap_or_default();
    let response = auth
        .mobile_api_request(
            &app,
            Method::GET,
            "/api/aokie-companion/mobile/call-records",
            &query,
            None,
        )
        .await?;
    parse_response(&response.body)
}

#[tauri::command]
pub async fn companion_call_record_detail(
    app: AppHandle,
    auth: State<'_, ManagedAuthState>,
    record_id: String,
) -> Result<CompanionCallRecordDetail, String> {
    validate_id(&record_id, "call record id")?;
    let response = auth
        .mobile_api_request(
            &app,
            Method::GET,
            &format!("/api/aokie-companion/mobile/call-records/{record_id}"),
            &[],
            None,
        )
        .await?;
    let parsed: CompanionCallRecordDetail = parse_response(&response.body)?;
    validate_call_record_detail_binding(&parsed, &record_id)?;
    Ok(parsed)
}

#[tauri::command]
pub async fn companion_availability(
    app: AppHandle,
    auth: State<'_, ManagedAuthState>,
) -> Result<CompanionAvailability, String> {
    let response = auth
        .mobile_api_request(
            &app,
            Method::GET,
            "/api/aokie-companion/mobile/availability",
            &[],
            None,
        )
        .await?;
    let parsed: CompanionAvailability = parse_response(&response.body)?;
    validate_native_availability_binding(&parsed, &response.app_id, &response.device_id)?;
    Ok(parsed)
}

#[tauri::command]
pub async fn companion_set_availability(
    app: AppHandle,
    auth: State<'_, ManagedAuthState>,
    availability: CompanionAvailabilityStatus,
    expires_in_seconds: Option<u32>,
) -> Result<CompanionAvailability, String> {
    if expires_in_seconds.is_some_and(|seconds| !(60..=86_400).contains(&seconds)) {
        return Err("Companion availability expiry must be 60 to 86400 seconds".into());
    }
    let body = serde_json::to_vec(&SetAvailabilityRequest {
        availability,
        expires_in_seconds,
    })
    .map_err(|_| "Companion availability could not be encoded".to_string())?;
    let response = auth
        .mobile_api_request(
            &app,
            Method::PUT,
            "/api/aokie-companion/mobile/availability",
            &[],
            Some(body),
        )
        .await?;
    let parsed: CompanionAvailability = parse_response(&response.body)?;
    validate_native_availability_binding(&parsed, &response.app_id, &response.device_id)?;
    Ok(parsed)
}

fn validate_native_availability_binding(
    response: &CompanionAvailability,
    expected_app_id: &str,
    expected_device_id: &str,
) -> Result<(), String> {
    if response.app_id != expected_app_id || response.device_id != expected_device_id {
        Err("Companion availability does not match the native managed session".into())
    } else {
        Ok(())
    }
}

fn parse_response<T>(bytes: &[u8]) -> Result<T, String>
where
    T: DeserializeOwned + ValidateResponse,
{
    let mut response: T = serde_json::from_slice(bytes)
        .map_err(|_| "Companion API response did not match its strict schema".to_string())?;
    response.sanitize();
    response.validate()?;
    Ok(response)
}

impl ValidateResponse for CompanionBootstrap {
    fn sanitize(&mut self) {
        self.device.sanitize();
        sanitize_groups(&mut self.routing_groups);
        sanitize_staff(&mut self.staff);
        self.history.sanitize();
        self.push_endpoints.retain(PushEndpoint::is_known);
    }

    fn validate(&self) -> Result<(), String> {
        validate_id(&self.membership.app_id, "membership appId")?;
        validate_id(&self.membership.app_slug, "membership appSlug")?;
        self.device.validate()?;
        if self.membership.app_id != self.device.app_id {
            return Err("Companion bootstrap app binding is inconsistent".into());
        }
        validate_capabilities(&self.capabilities)?;
        if self
            .capabilities
            .iter()
            .any(|capability| !self.device.grants.contains(capability))
        {
            return Err("Companion bootstrap capabilities exceed the device grants".into());
        }
        if let Some(availability) = &self.availability.0 {
            availability.validate()?;
        }
        validate_groups(&self.routing_groups)?;
        validate_staff(&self.staff)?;
        validate_routing_staff_consistency(&self.routing_groups, &self.staff)?;
        self.history.validate()?;
        if self.push_endpoints.len() > 3 {
            return Err("Companion bootstrap contains too many push endpoints".into());
        }
        for endpoint in &self.push_endpoints {
            endpoint.validate()?;
            if endpoint.app_id != self.device.app_id || endpoint.device_id != self.device.id {
                return Err("Companion push endpoint binding is inconsistent".into());
            }
        }
        Ok(())
    }
}

impl ValidateResponse for CompanionHistory {
    fn sanitize(&mut self) {
        self.activity.retain(|activity| {
            activity.event_type.is_known()
                && activity.mode.0.as_ref().is_none_or(Tolerant::is_known)
        });
        for activity in &mut self.activity {
            optional_display_line(&mut activity.reason, 120);
        }
        self.sessions
            .retain(|session| session.mode.is_known() && session.state.is_known());
        for session in &mut self.sessions {
            optional_display_line(&mut session.end_reason, 120);
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.activity.len() > 200 || self.sessions.len() > 200 {
            return Err("Companion history is too large".into());
        }
        for activity in &self.activity {
            activity.validate()?;
        }
        for session in &self.sessions {
            session.validate()?;
        }
        Ok(())
    }
}

impl ValidateResponse for CompanionRouting {
    fn sanitize(&mut self) {
        sanitize_groups(&mut self.routing_groups);
        sanitize_staff(&mut self.staff);
    }

    fn validate(&self) -> Result<(), String> {
        validate_groups(&self.routing_groups)?;
        validate_staff(&self.staff)?;
        validate_routing_staff_consistency(&self.routing_groups, &self.staff)
    }
}

impl ValidateResponse for CompanionCallRecords {
    fn sanitize(&mut self) {
        for record in &mut self.records {
            record.sanitize();
        }
        // One record that still fails (an unsafe id, an unredacted number) is
        // left out of the list rather than failing every other call in it.
        self.records.retain(|record| match record.validate() {
            Ok(()) => true,
            Err(message) => {
                eprintln!("[AokieCompanion][calls] call record left out: {message}");
                false
            }
        });
    }

    fn validate(&self) -> Result<(), String> {
        if self.records.len() > 100 {
            return Err("Companion call-record response is too large".into());
        }
        if self.access == CallRecordAccess::None && !self.records.is_empty() {
            return Err("Companion call-record access denied response contains records".into());
        }
        let mut ids = HashSet::with_capacity(self.records.len());
        for record in &self.records {
            record.validate()?;
            if !ids.insert(&record.id) {
                return Err("Companion call records are duplicated".into());
            }
        }
        Ok(())
    }
}

impl ValidateResponse for CompanionCallRecordDetail {
    fn sanitize(&mut self) {
        self.record.sanitize();
        for turn in &mut self.transcript {
            display_line(&mut turn.speaker, 64);
            display_line(&mut turn.text, 10_000);
        }
        for follow_up in &mut self.follow_ups {
            display_line(&mut follow_up.summary, 2_000);
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.record.validate()?;
        if self.transcript.len() > 200 || self.follow_ups.len() > 100 {
            return Err("Companion call-record detail is too large".into());
        }
        let mut transcript_ids = HashSet::with_capacity(self.transcript.len());
        for turn in &self.transcript {
            validate_id(&turn.id, "transcript turn id")?;
            if !transcript_ids.insert(&turn.id) {
                return Err("Companion transcript turns are duplicated".into());
            }
            validate_text(&turn.speaker, 64, "transcript speaker")?;
            validate_text(&turn.text, 10_000, "transcript text")?;
            validate_call_timestamp(&turn.occurred_at, "transcript occurredAt")?;
        }
        let mut follow_up_ids = HashSet::with_capacity(self.follow_ups.len());
        for follow_up in &self.follow_ups {
            validate_id(&follow_up.id, "follow-up id")?;
            if !follow_up_ids.insert(&follow_up.id) {
                return Err("Companion follow-ups are duplicated".into());
            }
            validate_text(&follow_up.summary, 2_000, "follow-up summary")?;
            validate_text(&follow_up.status, 64, "follow-up status")?;
            validate_text(&follow_up.priority, 64, "follow-up priority")?;
            validate_call_timestamp(&follow_up.submitted_at, "follow-up submittedAt")?;
        }
        Ok(())
    }
}

impl ValidateResponse for CompanionAvailability {
    fn validate(&self) -> Result<(), String> {
        validate_id(&self.app_id, "availability appId")?;
        validate_id(&self.device_id, "availability deviceId")?;
        if let Some(availability) = &self.availability.0 {
            availability.validate()?;
        }
        Ok(())
    }
}

impl CompanionDevice {
    fn sanitize(&mut self) {
        display_line(&mut self.display_name, 120);
    }

    fn validate(&self) -> Result<(), String> {
        for (value, label) in [
            (&self.id, "device id"),
            (&self.user_id, "device userId"),
            (&self.app_id, "device appId"),
            (&self.subject_id, "device subjectId"),
        ] {
            validate_id(value, label)?;
        }
        validate_text(&self.display_name, 120, "device displayName")?;
        validate_capabilities(&self.grants)?;
        validate_timestamp(&self.approved_at, "device approvedAt")?;
        validate_timestamp(&self.last_seen_at, "device lastSeenAt")?;
        if self.revoked_at.0.is_some() {
            return Err("active Companion device unexpectedly reports revocation".into());
        }
        Ok(())
    }
}

impl AvailabilityRecord {
    fn validate(&self) -> Result<(), String> {
        validate_timestamp(&self.updated_at, "availability updatedAt")?;
        validate_optional_timestamp(&self.expires_at, "availability expiresAt")
    }
}

impl TeamMember {
    fn validate(&self) -> Result<(), String> {
        validate_optional_id(&self.staff_id, "routing member staffId")?;
        validate_text(&self.display_name, 120, "routing member displayName")?;
        if let Some(role_name) = &self.role_name.0 {
            validate_text(role_name, 120, "routing member roleName")?;
        }
        if self.priority > 1_000_000 {
            return Err("routing member priority is invalid".into());
        }
        if self.is_current_device && !self.is_current_user {
            return Err("routing member current device is not owned by the current user".into());
        }
        validate_timestamp(
            &self.availability_updated_at,
            "routing member availabilityUpdatedAt",
        )?;
        validate_optional_timestamp(
            &self.availability_expires_at,
            "routing member availabilityExpiresAt",
        )
    }
}

impl StaffMember {
    fn validate(&self) -> Result<(), String> {
        validate_id(&self.id, "staff id")?;
        validate_text(&self.display_name, 120, "staff displayName")?;
        validate_text(&self.role_name, 120, "staff roleName")
    }
}

impl CallRecord {
    fn sanitize(&mut self) {
        optional_display_line(&mut self.caller_name, 200);
        optional_display_line(&mut self.summary, 5_000);
    }

    fn validate(&self) -> Result<(), String> {
        validate_id(&self.id, "call record id")?;
        validate_id(&self.call_id, "call record callId")?;
        if let Some(name) = &self.caller_name.0 {
            validate_text(name, 200, "call record callerName")?;
        }
        if let Some(number) = &self.masked_number.0 {
            validate_text(number, 40, "call record maskedNumber")?;
            if number.chars().filter(char::is_ascii_digit).count() > 4 {
                return Err("call record number is not sufficiently redacted".into());
            }
        }
        validate_text(&self.status, 64, "call record status")?;
        if let Some(summary) = &self.summary.0 {
            validate_text(summary, 5_000, "call record summary")?;
        }
        validate_optional_call_timestamp(&self.started_at, "call record startedAt")?;
        validate_optional_call_timestamp(&self.ended_at, "call record endedAt")?;
        if self
            .duration_seconds
            .0
            .is_some_and(|value| value > MAX_SAFE_INTEGER)
        {
            return Err("call record duration is invalid".into());
        }
        validate_call_timestamp(&self.submitted_at, "call record submittedAt")
    }
}

impl CompanionActivity {
    fn validate(&self) -> Result<(), String> {
        validate_id(&self.id, "activity id")?;
        validate_id(&self.event_id, "activity eventId")?;
        validate_id(&self.app_id, "activity appId")?;
        validate_id(&self.subject_id, "activity subjectId")?;
        for (value, label) in [
            (&self.session_record_id, "activity sessionRecordId"),
            (&self.call_id, "activity callId"),
            (&self.device_id, "activity deviceId"),
            (&self.actor_user_id, "activity actorUserId"),
        ] {
            validate_optional_id(value, label)?;
        }
        if let Some(reason) = &self.reason.0 {
            validate_text(reason, 120, "activity reason")?;
        }
        if self
            .owner_epoch
            .0
            .is_some_and(|epoch| epoch > MAX_SAFE_INTEGER)
        {
            return Err("activity ownerEpoch is invalid".into());
        }
        validate_timestamp(&self.occurred_at, "activity occurredAt")
    }
}

impl CompanionSession {
    fn validate(&self) -> Result<(), String> {
        for (value, label) in [
            (&self.id, "session id"),
            (&self.session_id, "session sessionId"),
            (&self.call_id, "session callId"),
            (&self.subject_id, "session subjectId"),
            (&self.last_event_id, "session lastEventId"),
        ] {
            validate_id(value, label)?;
        }
        validate_optional_id(&self.device_id, "session deviceId")?;
        validate_optional_timestamp(&self.joined_at, "session joinedAt")?;
        validate_optional_timestamp(&self.ended_at, "session endedAt")?;
        if let Some(reason) = &self.end_reason.0 {
            validate_text(reason, 120, "session endReason")?;
        }
        validate_timestamp(&self.last_event_at, "session lastEventAt")
    }
}

impl PushEndpoint {
    fn is_known(&self) -> bool {
        self.kind.is_known()
            && self.mode.is_known()
            && self.provider.is_known()
            && self.environment.is_known()
    }

    fn validate(&self) -> Result<(), String> {
        validate_id(&self.id, "push endpoint id")?;
        validate_id(&self.app_id, "push endpoint appId")?;
        validate_id(&self.device_id, "push endpoint deviceId")?;
        if let Some(topic) = &self.topic.0 {
            validate_text(topic, 255, "push endpoint topic")?;
        }
        if self.fingerprint.len() != 64
            || !self
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("push endpoint fingerprint is invalid".into());
        }
        validate_optional_timestamp(&self.invalidated_at, "push endpoint invalidatedAt")?;
        validate_timestamp(&self.rotated_at, "push endpoint rotatedAt")
    }
}

fn sanitize_groups(groups: &mut [RoutingGroup]) {
    for group in groups {
        display_line(&mut group.name, 120);
        for member in &mut group.members {
            display_line(&mut member.display_name, 120);
            optional_display_line(&mut member.role_name, 120);
        }
    }
}

fn sanitize_staff(staff: &mut [StaffMember]) {
    for member in staff {
        display_line(&mut member.display_name, 120);
        display_line(&mut member.role_name, 120);
    }
}

fn validate_groups(groups: &[RoutingGroup]) -> Result<(), String> {
    if groups.len() > 200 {
        return Err("Companion routing contains too many groups".into());
    }
    let mut ids = HashSet::with_capacity(groups.len());
    for group in groups {
        validate_id(&group.id, "routing group id")?;
        validate_text(&group.name, 120, "routing group name")?;
        if !ids.insert(&group.id) || group.members.len() > 200 {
            return Err("Companion routing group is duplicated or too large".into());
        }
        if group
            .members
            .iter()
            .filter(|member| member.is_current_device)
            .count()
            > 1
        {
            return Err("Companion routing group identifies multiple current devices".into());
        }
        for member in &group.members {
            member.validate()?;
        }
    }
    Ok(())
}

fn validate_staff(staff: &[StaffMember]) -> Result<(), String> {
    if staff.len() > 200 {
        return Err("Companion staff directory is too large".into());
    }
    let mut ids = HashSet::with_capacity(staff.len());
    let mut current = 0usize;
    for member in staff {
        member.validate()?;
        if !ids.insert(&member.id) {
            return Err("Companion staff directory is duplicated".into());
        }
        current += usize::from(member.is_current_user);
    }
    if !staff.is_empty() && current != 1 {
        return Err("Companion staff directory must identify exactly one current user".into());
    }
    Ok(())
}

fn validate_routing_staff_consistency(
    groups: &[RoutingGroup],
    staff: &[StaffMember],
) -> Result<(), String> {
    for member in groups.iter().flat_map(|group| group.members.iter()) {
        let Some(staff_id) = member.staff_id.0.as_deref() else {
            continue;
        };
        let Some(staff_member) = staff
            .iter()
            .find(|staff_member| staff_member.id == staff_id)
        else {
            // The directory is intentionally capped. A valid routing identity
            // may therefore be outside this response and cannot be compared.
            continue;
        };
        if member.display_name != staff_member.display_name
            || member.role_name.0.as_deref() != Some(staff_member.role_name.as_str())
            || member.is_current_user != staff_member.is_current_user
        {
            return Err("Companion routing staff identity is inconsistent".into());
        }
    }
    Ok(())
}

fn validate_call_record_detail_binding(
    detail: &CompanionCallRecordDetail,
    expected_record_id: &str,
) -> Result<(), String> {
    if detail.record.id == expected_record_id {
        Ok(())
    } else {
        Err("Companion call-record detail does not match the requested record".into())
    }
}

fn validate_capabilities(capabilities: &[CompanionCapability]) -> Result<(), String> {
    if capabilities.is_empty() || capabilities.len() > MAX_COMPANION_CAPABILITIES {
        return Err("Companion capability list is invalid".into());
    }
    let unique: HashSet<_> = capabilities.iter().collect();
    if unique.len() != capabilities.len() || !capabilities.contains(&CompanionCapability::StateRead)
    {
        return Err("Companion capability list is duplicated or lacks state_read".into());
    }
    Ok(())
}

fn validate_id(value: &str, label: &str) -> Result<(), String> {
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

fn validate_optional_id(value: &RequiredNullable<String>, label: &str) -> Result<(), String> {
    match &value.0 {
        Some(value) => validate_id(value, label),
        None => Ok(()),
    }
}

fn validate_text(value: &str, maximum: usize, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(format!("{label} is invalid"))
    } else {
        Ok(())
    }
}

fn validate_timestamp(value: &str, label: &str) -> Result<(), String> {
    let legacy_database_utc =
        value.len() == 19 && NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").is_ok();
    if legacy_database_utc || chrono::DateTime::parse_from_rfc3339(value).is_ok() {
        Ok(())
    } else {
        Err(format!("{label} is invalid"))
    }
}

fn validate_call_timestamp(value: &str, label: &str) -> Result<(), String> {
    validate_timestamp(value, label)
}

fn validate_optional_call_timestamp(
    value: &RequiredNullable<String>,
    label: &str,
) -> Result<(), String> {
    match &value.0 {
        Some(value) => validate_call_timestamp(value, label),
        None => Ok(()),
    }
}

fn validate_optional_timestamp(
    value: &RequiredNullable<String>,
    label: &str,
) -> Result<(), String> {
    match &value.0 {
        Some(value) => validate_timestamp(value, label),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn availability_json() -> &'static str {
        r#"{
          "appId":"app_1","deviceId":"installation_1",
          "availability":{"availability":"available","updatedAt":"2026-07-16 03:00:00","expiresAt":null}
        }"#
    }

    fn call_detail_json() -> &'static str {
        r#"{
          "record":{
            "id":"record_1","callId":"call_1","callerName":null,"maskedNumber":null,
            "status":"completed","direction":"inbound","summary":"Booking captured.",
            "startedAt":"2026-07-16T03:00:00Z","endedAt":"2026-07-16T03:04:00Z",
            "durationSeconds":240,"followUpRequired":false,"submittedAt":"2026-07-16 03:00:00"
          },
          "transcript":[
            {"id":"turn_1","speaker":"caller","text":"Hello","occurredAt":"2026-07-16T03:00:01Z"}
          ],
          "followUps":[
            {"id":"task_1","summary":"Call back","status":"open","priority":"high","submittedAt":"2026-07-16 03:01:00"}
          ]
        }"#
    }

    #[test]
    fn availability_schema_is_exact_and_bounded() {
        let parsed: CompanionAvailability = parse_response(availability_json().as_bytes()).unwrap();
        assert_eq!(parsed.app_id, "app_1");
        let unknown = availability_json().replace(
            "\"deviceId\":\"installation_1\"",
            "\"deviceId\":\"installation_1\",\"accessToken\":\"secret\"",
        );
        assert!(parse_response::<CompanionAvailability>(unknown.as_bytes()).is_err());
        let canonical = availability_json().replace("2026-07-16 03:00:00", "2026-07-16T03:00:00Z");
        assert!(parse_response::<CompanionAvailability>(canonical.as_bytes()).is_ok());
        let bad_time = availability_json().replace("2026-07-16 03:00:00", "2026-07-16T03:00:00");
        assert!(parse_response::<CompanionAvailability>(bad_time.as_bytes()).is_err());
    }

    #[test]
    fn required_nullable_fields_cannot_be_omitted() {
        let missing = availability_json().replace(",\"expiresAt\":null", "");
        assert!(parse_response::<CompanionAvailability>(missing.as_bytes()).is_err());
    }

    #[test]
    fn participant_capabilities_match_the_managed_api_schema() {
        let capabilities: Vec<CompanionCapability> = serde_json::from_str(
            r#"[
              "state_read","caller_read","captions_read","participants_read",
              "participant_identity_read","audio_levels_read","monitor","consult",
              "takeover","resume_aokie","rtc_signal","assistance_read",
              "assistance_respond","end_caller"
            ]"#,
        )
        .unwrap();

        assert_eq!(capabilities.len(), MAX_COMPANION_CAPABILITIES);
        assert!(capabilities.contains(&CompanionCapability::ParticipantsRead));
        assert!(capabilities.contains(&CompanionCapability::ParticipantIdentityRead));
        assert!(capabilities.contains(&CompanionCapability::AudioLevelsRead));
        assert!(validate_capabilities(&capabilities).is_ok());
    }

    #[test]
    fn history_leaves_out_entries_whose_kind_comes_from_a_newer_server() {
        let history = br#"{
          "activity":[{
            "id":"row_1","eventId":"event_1","appId":"app_1","sessionRecordId":null,
            "callId":null,"deviceId":null,"actorUserId":null,"subjectId":"device_1",
            "eventType":"caller_transcript","mode":null,"reason":null,"ownerEpoch":null,
            "occurredAt":"2026-07-16 03:00:00"
          },{
            "id":"row_2","eventId":"event_2","appId":"app_1","sessionRecordId":null,
            "callId":null,"deviceId":null,"actorUserId":null,"subjectId":"device_1",
            "eventType":"monitor_joined","mode":"monitor","reason":"Joined\nfrom the lock screen","ownerEpoch":null,
            "occurredAt":"2026-07-16 03:00:00"
          }],"sessions":[{
            "id":"s_1","sessionId":"session_1","callId":"call_1","deviceId":null,
            "subjectId":"device_1","mode":"whisper","state":"joined","joinedAt":null,
            "endedAt":null,"endReason":null,"lastEventId":"event_2","lastEventAt":"2026-07-16 03:00:00"
          }]
        }"#;
        let parsed = parse_response::<CompanionHistory>(history).unwrap();
        assert_eq!(parsed.activity.len(), 1);
        assert_eq!(parsed.activity[0].id, "row_2");
        assert_eq!(
            parsed.activity[0].reason.0.as_deref(),
            Some("Joined from the lock screen")
        );
        assert!(parsed.sessions.is_empty());
        // What reaches the WebView carries only names it knows.
        let encoded = serde_json::to_string(&parsed).unwrap();
        assert!(!encoded.contains("caller_transcript") && !encoded.contains("whisper"));

        // A malformed enum value is still refused outright.
        let malformed = String::from_utf8(history.to_vec())
            .unwrap()
            .replace("caller_transcript", "caller transcript");
        assert!(parse_response::<CompanionHistory>(malformed.as_bytes()).is_err());
    }

    #[test]
    fn capability_names_from_a_newer_server_are_dropped_not_fatal() {
        let capabilities = deserialize_known_capabilities(&mut serde_json::Deserializer::from_str(
            r#"["state_read","notes_read","monitor"]"#,
        ))
        .unwrap();
        assert_eq!(
            capabilities,
            vec![CompanionCapability::StateRead, CompanionCapability::Monitor]
        );
        assert!(
            deserialize_known_capabilities(&mut serde_json::Deserializer::from_str(
                r#"["state_read","not a name"]"#
            ))
            .is_err()
        );
    }

    #[test]
    fn availability_request_omits_optional_expiry() {
        let encoded = serde_json::to_value(SetAvailabilityRequest {
            availability: CompanionAvailabilityStatus::DoNotDisturb,
            expires_in_seconds: None,
        })
        .unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({"availability":"do_not_disturb"})
        );
    }

    #[test]
    fn call_records_require_masked_numbers_and_exact_fields() {
        let records = br#"{
          "records":[{
            "id":"record_1","callId":"call_1","callerName":"Customer","maskedNumber":"... 782",
            "status":"completed","direction":"inbound","summary":"Booking captured.",
            "startedAt":"2026-07-16T03:00:00Z","endedAt":"2026-07-16T03:04:00Z",
            "durationSeconds":240,"followUpRequired":false,"submittedAt":"2026-07-16 03:00:00"
          }],"access":"full"
        }"#;
        assert!(parse_response::<CompanionCallRecords>(records).is_ok());
        // An unredacted number never reaches the screen: that record is left
        // out of the list.
        let unredacted = String::from_utf8(records.to_vec())
            .unwrap()
            .replace("... 782", "+61 491 570 156");
        assert!(
            parse_response::<CompanionCallRecords>(unredacted.as_bytes())
                .unwrap()
                .records
                .is_empty()
        );
        let secret = String::from_utf8(records.to_vec()).unwrap().replace(
            "\"access\":\"full\"",
            "\"access\":\"full\",\"accessToken\":\"secret\"",
        );
        assert!(parse_response::<CompanionCallRecords>(secret.as_bytes()).is_err());
        let denied_with_records = String::from_utf8(records.to_vec())
            .unwrap()
            .replace("\"access\":\"full\"", "\"access\":\"none\"");
        assert!(parse_response::<CompanionCallRecords>(denied_with_records.as_bytes()).is_err());
    }

    #[test]
    fn one_multi_line_summary_or_bad_record_no_longer_fails_the_calls_list() {
        let record = |id: &str, number: &str, summary: &str| {
            serde_json::json!({
                "id": id, "callId": format!("call_{id}"), "callerName": "Customer\tName",
                "maskedNumber": number, "status": "completed", "direction": "internal",
                "summary": summary, "startedAt": "2026-07-16T03:00:00Z",
                "endedAt": "2026-07-16T03:04:00Z", "durationSeconds": 240,
                "followUpRequired": false, "submittedAt": "2026-07-16 03:00:00"
            })
        };
        let long_cjk = "予約".repeat(1_500);
        let body = serde_json::json!({
            "records": [
                record("record_1", "... 782", "Booked Friday.\nCall back Thursday."),
                record("record_2", "+61 491 570 156", "Unredacted number"),
                record("record_3", "... 123", &long_cjk),
                record("record_4", "... 456", "\n\n"),
            ],
            "access": "full"
        });
        let parsed = parse_response::<CompanionCallRecords>(body.to_string().as_bytes()).unwrap();
        let ids: Vec<_> = parsed
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        assert_eq!(ids, ["record_1", "record_3", "record_4"]);
        assert_eq!(
            parsed.records[0].summary.0.as_deref(),
            Some("Booked Friday. Call back Thursday.")
        );
        assert_eq!(
            parsed.records[0].caller_name.0.as_deref(),
            Some("Customer Name")
        );
        assert!(matches!(
            parsed.records[0].direction,
            CallDirection::Unknown
        ));
        let cut = parsed.records[1].summary.0.as_deref().unwrap();
        assert!(cut.len() <= 5_000 && long_cjk.starts_with(cut));
        // A summary of nothing but line breaks reads as no summary.
        assert_eq!(parsed.records[2].summary.0, None);
    }

    #[test]
    fn call_detail_rejects_duplicate_children_and_wrong_request_binding() {
        let detail: CompanionCallRecordDetail =
            parse_response(call_detail_json().as_bytes()).unwrap();
        assert!(validate_call_record_detail_binding(&detail, "record_1").is_ok());
        assert!(validate_call_record_detail_binding(&detail, "record_other").is_err());

        let mut duplicate_turn: serde_json::Value =
            serde_json::from_str(call_detail_json()).unwrap();
        let turn = duplicate_turn["transcript"][0].clone();
        duplicate_turn["transcript"]
            .as_array_mut()
            .unwrap()
            .push(turn);
        assert!(parse_response::<CompanionCallRecordDetail>(
            serde_json::to_string(&duplicate_turn).unwrap().as_bytes()
        )
        .is_err());

        let mut duplicate_follow_up: serde_json::Value =
            serde_json::from_str(call_detail_json()).unwrap();
        let follow_up = duplicate_follow_up["followUps"][0].clone();
        duplicate_follow_up["followUps"]
            .as_array_mut()
            .unwrap()
            .push(follow_up);
        assert!(parse_response::<CompanionCallRecordDetail>(
            serde_json::to_string(&duplicate_follow_up)
                .unwrap()
                .as_bytes()
        )
        .is_err());
    }

    #[test]
    fn staff_directory_rejects_duplicate_or_multiple_current_members() {
        let duplicate = vec![
            StaffMember {
                id: "staff_1".into(),
                display_name: "One".into(),
                role_name: "Owner".into(),
                is_current_user: true,
                is_owner: true,
                companion_ready: true,
            },
            StaffMember {
                id: "staff_1".into(),
                display_name: "Two".into(),
                role_name: "Receptionist".into(),
                is_current_user: true,
                is_owner: false,
                companion_ready: false,
            },
        ];
        assert!(validate_staff(&duplicate).is_err());

        let mut no_current = duplicate;
        no_current[0].id = "staff_1".into();
        no_current[1].id = "staff_2".into();
        no_current[0].is_current_user = false;
        no_current[1].is_current_user = false;
        assert!(validate_staff(&no_current).is_err());
    }

    #[test]
    fn routing_staff_identity_matches_known_directory_members() {
        let routing = r#"{
          "routingGroups":[{
            "id":"group_1","name":"Primary","policy":"priority","enabled":true,
            "members":[{
              "staffId":"staff_1","displayName":"Test User","roleName":"Owner",
              "isCurrentUser":true,"isCurrentDevice":true,"priority":1,"enabled":true,"availability":"available",
              "availabilityUpdatedAt":"2026-07-16 03:00:00","availabilityExpiresAt":null
            }]
          }],
          "staff":[{
            "id":"staff_1","displayName":"Test User","roleName":"Owner",
            "isCurrentUser":true,"isOwner":true,"companionReady":true
          }]
        }"#;
        assert!(parse_response::<CompanionRouting>(routing.as_bytes()).is_ok());

        let legacy = routing.replace("\"isCurrentDevice\":true,", "");
        let parsed_legacy = parse_response::<CompanionRouting>(legacy.as_bytes())
            .expect("schema-1 routing remains rollback-compatible");
        assert!(!parsed_legacy.routing_groups[0].members[0].is_current_device);

        let mismatched = routing.replace(
            "\"displayName\":\"Test User\",\"roleName\":\"Owner\",\n              \"isCurrentUser\":true,\"isCurrentDevice\":true,\"priority\"",
            "\"displayName\":\"Wrong User\",\"roleName\":\"Owner\",\n              \"isCurrentUser\":true,\"isCurrentDevice\":true,\"priority\"",
        );
        assert!(parse_response::<CompanionRouting>(mismatched.as_bytes()).is_err());

        let capped_directory_member = routing.replace(
            "\"staffId\":\"staff_1\"",
            "\"staffId\":\"staff_outside_cap\"",
        );
        assert!(parse_response::<CompanionRouting>(capped_directory_member.as_bytes()).is_ok());

        let foreign_current_device = routing.replace(
            "\"isCurrentUser\":true,\"isCurrentDevice\":true,\"priority\"",
            "\"isCurrentUser\":false,\"isCurrentDevice\":true,\"priority\"",
        );
        assert!(parse_response::<CompanionRouting>(foreign_current_device.as_bytes()).is_err());
    }
}
