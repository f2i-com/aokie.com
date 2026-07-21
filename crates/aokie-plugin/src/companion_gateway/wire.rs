//! Wire envelopes and authoritative media/telemetry composition.

#[allow(unused_imports)]
use super::*;

pub(super) fn map_service_mode(mode: LocalServiceMode) -> ProtocolServiceMode {
    match mode {
        LocalServiceMode::AokieActive => ProtocolServiceMode::AokieActive,
        LocalServiceMode::SoftHold => ProtocolServiceMode::SoftHold,
        LocalServiceMode::ConsultPending => ProtocolServiceMode::ConsultPending,
        LocalServiceMode::ConsultActive => ProtocolServiceMode::ConsultActive,
        LocalServiceMode::HumanPending => ProtocolServiceMode::HumanPending,
        LocalServiceMode::HumanActive => ProtocolServiceMode::HumanActive,
        LocalServiceMode::ReturningToAokie => ProtocolServiceMode::ReturningToAokie,
        LocalServiceMode::Recovering => ProtocolServiceMode::Recovering,
    }
}

pub(super) fn authoritative_media_state(
    remote: &crate::remote_media::RemoteMediaSnapshot,
    physical_call_active: bool,
) -> MediaState {
    match remote.service_mode {
        LocalServiceMode::ConsultActive => MediaState::Active,
        LocalServiceMode::HumanActive if remote.talk_audio_forwarded => MediaState::Active,
        LocalServiceMode::HumanActive => MediaState::Connecting,
        LocalServiceMode::SoftHold
        | LocalServiceMode::ConsultPending
        | LocalServiceMode::HumanPending
        | LocalServiceMode::ReturningToAokie
        | LocalServiceMode::Recovering => MediaState::Connecting,
        _ if remote.peer_count > 0 => MediaState::Receiving,
        _ if physical_call_active => MediaState::Ready,
        _ => MediaState::None,
    }
}

pub(super) fn remote_consent_is_current(enabled: bool, acknowledged: bool, expires_at: Option<&str>) -> bool {
    enabled
        && acknowledged
        && expires_at.is_none_or(|expiry| {
            chrono::DateTime::parse_from_rfc3339(expiry)
                .is_ok_and(|expiry| expiry > chrono::Utc::now())
        })
}

/// Builds the socket-authoritative telemetry projection. Participant presence
/// and audio activity are consented live-call data, so neither may survive a
/// disabled, unacknowledged, or expired remote-access policy—even before the
/// relay applies each endpoint's narrower grant projection.
pub(super) fn authoritative_remote_telemetry(
    remote: &crate::remote_media::RemoteMediaSnapshot,
) -> (Vec<ParticipantPresence>, Option<Vec<NormalizedAudioLevel>>) {
    if !remote_consent_is_current(
        remote.consent.enabled,
        remote.consent.acknowledged,
        remote.consent.expires_at.as_deref(),
    ) {
        return (Vec::new(), None);
    }
    let participants = remote
        .participants
        .iter()
        .map(|participant| ParticipantPresence {
            participant_id: participant.participant_id.clone(),
            mode: match participant.mode {
                MediaMode::Monitor => ParticipantMode::Observer,
                MediaMode::PreparedConsult | MediaMode::Consult => ParticipantMode::Advisor,
                MediaMode::PreparedTalk | MediaMode::Talk => ParticipantMode::Talker,
            },
            state: match participant.state {
                RemoteParticipantState::Connected => ParticipantState::Connected,
                RemoteParticipantState::Prepared => ParticipantState::Prepared,
                RemoteParticipantState::Active => ParticipantState::Active,
            },
            subject_id: Some(participant.device_id.clone()),
            display_label: Some("Owner Companion".into()),
        })
        .collect();
    let audio_levels = (!remote.audio_levels.is_empty()).then(|| {
        remote
            .audio_levels
            .iter()
            .map(|level| NormalizedAudioLevel {
                source: match level.source {
                    RemoteAudioLevelSource::Caller => AudioLevelSource::Caller,
                    RemoteAudioLevelSource::Aokie => AudioLevelSource::Aokie,
                    RemoteAudioLevelSource::Companion => AudioLevelSource::Companion,
                },
                participant_id: level.participant_id.clone(),
                level_permille: level.level_permille,
            })
            .collect()
    });
    (participants, audio_levels)
}

pub(super) fn remote_media_event_kind(kind: &RemoteMediaEventKind) -> &'static str {
    match kind {
        RemoteMediaEventKind::SdpAnswer { .. } => "sdp_answer",
        RemoteMediaEventKind::LocalIce { .. } => "local_ice",
        RemoteMediaEventKind::IceComplete => "ice_complete",
        RemoteMediaEventKind::ConnectionState { .. } => "connection_state",
        RemoteMediaEventKind::RemoteAudioReady => "remote_audio_ready",
        RemoteMediaEventKind::RemoteMicrophoneReady => "remote_microphone_ready",
        RemoteMediaEventKind::ProtocolViolation { .. } => "protocol_violation",
        RemoteMediaEventKind::TakeoverPending => "takeover_pending",
        RemoteMediaEventKind::TakeoverPrepared { .. } => "takeover_prepared",
        RemoteMediaEventKind::ConsultPrepared { .. } => "consult_prepared",
        RemoteMediaEventKind::ConsultActive => "consult_active",
        RemoteMediaEventKind::HumanActive => "human_active",
        RemoteMediaEventKind::ReturningToAokie { .. } => "returning_to_aokie",
        RemoteMediaEventKind::AokieActive => "aokie_active",
        RemoteMediaEventKind::Closed { .. } => "closed",
        RemoteMediaEventKind::Error { .. } => "error",
    }
}

pub(super) fn mask_number(number: &str) -> Option<String> {
    let digits = number
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        let suffix = digits
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        Some(format!("***{suffix}"))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Envelope {
    pub(super) kind: String,
    pub(super) schema_version: u16,
    #[serde(default)]
    pub(super) app_id: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LeaseNotice {
    pub(super) kind: String,
    pub(super) schema_version: u16,
    pub(super) app_id: String,
    #[serde(default)]
    pub(super) request_id: Option<String>,
    pub(super) device_id: String,
    pub(super) lease_token: String,
    pub(super) lease: LeaseClaims,
    #[serde(default)]
    pub(super) accepted_transfer_request_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LeaseRevokedNotice {
    pub(super) kind: String,
    pub(super) schema_version: u16,
    pub(super) app_id: String,
    pub(super) device_id: String,
    pub(super) lease_id: String,
    pub(super) lease_jti: String,
    pub(super) call_id: String,
    pub(super) call_epoch: u64,
    pub(super) fence: u64,
    pub(super) reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ErrorNotice {
    pub(super) kind: String,
    pub(super) schema_version: u16,
    pub(super) code: String,
    pub(super) message: String,
    #[serde(default)]
    pub(super) request_id: Option<String>,
}
