//! Direction-specific realtime signalling and media-lease contract.
//!
//! V2 intentionally keeps SDP and ICE ephemeral.  Only the Aokie endpoint and
//! the selected Companion receive them; the gateway validates and relays the
//! frames but never handles media.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: u16 = 2;
pub const LEASE_AUDIENCE: &str = "aokie-companion-media";
pub const ADMISSION_AUDIENCE: &str = "aokie-v2-gateway";
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const MAX_ID_BYTES: usize = 200;
pub const MAX_IDEMPOTENCY_BYTES: usize = 200;
pub const MAX_SDP_BYTES: usize = 128 * 1024;
pub const MAX_ICE_BYTES: usize = 4 * 1024;
pub const MAX_REASON_BYTES: usize = 500;
pub const MAX_CAPTIONS: usize = 200;
pub const MAX_CAPTION_BYTES: usize = 2_000;
pub const MAX_LEASE_TOKEN_BYTES: usize = 16 * 1024;
pub const MAX_ASSISTANCE_QUESTION_BYTES: usize = 1_000;
pub const MAX_ASSISTANCE_CONTEXT_BYTES: usize = 2_000;
pub const MAX_ASSISTANCE_ANSWER_BYTES: usize = 2_000;
pub const ENDPOINT_PROOF_MAX_LIFETIME: u64 = 30;
pub const ENDPOINT_PROOF_CLOCK_SKEW: u64 = 5;
pub const MOBILE_OFFER_MAX_LIFETIME: u64 = 30;
pub const MAX_PENDING_MOBILE_OFFERS: usize = 8;
pub const ENDPOINT_PUBLIC_KEY_BYTES: usize = 32;
pub const ENDPOINT_SIGNATURE_BYTES: usize = 64;
pub const MAX_APPROVED_PEER_KEYS: usize = 64;
pub const MAX_PARTICIPANTS: usize = 64;
pub const MAX_AUDIO_LEVELS: usize = 64;
const HELLO_PROOF_DOMAIN: &str = "aokie/v2/hello-proof";
const ENDPOINT_BINDING_DOMAIN: &str = "aokie/v2/endpoint-binding";
const TRICKLE_CANDIDATE_DOMAIN: &str = "aokie/v2/trickle-candidate";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    StateRead,
    CallerRead,
    CaptionsRead,
    AssistanceRead,
    AssistanceRespond,
    Monitor,
    Consult,
    Takeover,
    EndCaller,
    ResumeAokie,
    RtcSignal,
    ParticipantsRead,
    ParticipantIdentityRead,
    AudioLevelsRead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndCallerOutcome {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionRole {
    Mobile,
    Plugin,
}

/// Algorithm tag for a long-lived endpoint identity. Managed beta endpoints
/// use software Ed25519 keys; the tag leaves the wire contract ready for a
/// hardware-backed algorithm without silently interpreting key bytes under a
/// different primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKeyAlgorithm {
    Ed25519,
}

/// Public half of an endpoint identity. `thumbprint` is an RFC 7638-style
/// base64url SHA-256 digest over the canonical OKP JWK members and must equal
/// the value recomputed from `public_key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointPublicKey {
    pub algorithm: EndpointKeyAlgorithm,
    pub public_key: String,
    pub thumbprint: String,
}

impl EndpointPublicKey {
    pub fn from_ed25519_bytes(bytes: &[u8; ENDPOINT_PUBLIC_KEY_BYTES]) -> Self {
        let public_key = URL_SAFE_NO_PAD.encode(bytes);
        let thumbprint = endpoint_thumbprint(&public_key);
        Self {
            algorithm: EndpointKeyAlgorithm::Ed25519,
            public_key,
            thumbprint,
        }
    }

    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.public_key)
            .map_err(|_| V2ProtocolError::Invalid("endpointKey.publicKey"))?;
        let bytes: [u8; ENDPOINT_PUBLIC_KEY_BYTES] = bytes
            .try_into()
            .map_err(|_| V2ProtocolError::Invalid("endpointKey.publicKey"))?;
        VerifyingKey::from_bytes(&bytes)
            .map_err(|_| V2ProtocolError::Invalid("endpointKey.publicKey"))?;
        safe_id("endpointKey.thumbprint", &self.thumbprint)?;
        if self.thumbprint != endpoint_thumbprint(&self.public_key) {
            return Err(V2ProtocolError::Invalid("endpointKey.thumbprint"));
        }
        Ok(())
    }

    pub fn verify(&self, message: &[u8], encoded_signature: &str) -> Result<(), V2ProtocolError> {
        self.validate()?;
        let public_key = URL_SAFE_NO_PAD
            .decode(&self.public_key)
            .map_err(|_| V2ProtocolError::Invalid("endpointKey.publicKey"))?;
        let public_key: [u8; ENDPOINT_PUBLIC_KEY_BYTES] = public_key
            .try_into()
            .map_err(|_| V2ProtocolError::Invalid("endpointKey.publicKey"))?;
        let signature = URL_SAFE_NO_PAD
            .decode(encoded_signature)
            .map_err(|_| V2ProtocolError::Invalid("signature"))?;
        let signature: [u8; ENDPOINT_SIGNATURE_BYTES] = signature
            .try_into()
            .map_err(|_| V2ProtocolError::Invalid("signature"))?;
        VerifyingKey::from_bytes(&public_key)
            .map_err(|_| V2ProtocolError::Invalid("endpointKey.publicKey"))?
            .verify(message, &Signature::from_bytes(&signature))
            .map_err(|_| V2ProtocolError::Unsafe("endpoint signature verification failed"))
    }
}

fn endpoint_thumbprint(public_key: &str) -> String {
    // RFC 7638 required OKP members in lexicographic order. The endpoint key
    // algorithm is validated separately and is not a JWK thumbprint member.
    let canonical = format!(
        "{{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":{}}}",
        serde_json::to_string(public_key).expect("string serialization cannot fail")
    );
    URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointChallengeFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub subject_id: String,
    pub role: AdmissionRole,
    pub connection_id: String,
    pub challenge_nonce: String,
    pub admission_jti: String,
    pub holder_key_thumbprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_peer_key_thumbprint: Option<String>,
    #[serde(default)]
    pub approved_peer_key_thumbprints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_roster_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_roster_hash: Option<String>,
    pub expires_at: u64,
}

impl EndpointChallengeFrame {
    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "endpoint_challenge")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("subjectId", &self.subject_id)?;
        safe_id("connectionId", &self.connection_id)?;
        safe_id("challengeNonce", &self.challenge_nonce)?;
        safe_id("admissionJti", &self.admission_jti)?;
        safe_id("holderKeyThumbprint", &self.holder_key_thumbprint)?;
        validate_peer_policy(
            self.role,
            &self.holder_key_thumbprint,
            self.expected_peer_key_thumbprint.as_deref(),
            &self.approved_peer_key_thumbprints,
            self.peer_roster_revision,
            self.peer_roster_hash.as_deref(),
        )?;
        safe_integer("expiresAt", self.expires_at, 1)?;
        if self.expires_at <= now_unix
            || self.expires_at.saturating_sub(now_unix) > ENDPOINT_PROOF_MAX_LIFETIME
        {
            return Err(V2ProtocolError::Expired);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelloProofClaims {
    pub app_id: String,
    pub subject_id: String,
    pub role: AdmissionRole,
    pub connection_id: String,
    pub challenge_nonce: String,
    pub admission_jti: String,
    pub session_nonce: String,
    pub holder_key_thumbprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_peer_key_thumbprint: Option<String>,
    #[serde(default)]
    pub approved_peer_key_thumbprints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_roster_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_roster_hash: Option<String>,
    pub nonce: String,
    pub jti: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

impl HelloProofClaims {
    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        safe_id("appId", &self.app_id)?;
        safe_id("subjectId", &self.subject_id)?;
        safe_id("connectionId", &self.connection_id)?;
        safe_id("challengeNonce", &self.challenge_nonce)?;
        safe_id("admissionJti", &self.admission_jti)?;
        safe_id("sessionNonce", &self.session_nonce)?;
        safe_id("holderKeyThumbprint", &self.holder_key_thumbprint)?;
        validate_peer_policy(
            self.role,
            &self.holder_key_thumbprint,
            self.expected_peer_key_thumbprint.as_deref(),
            &self.approved_peer_key_thumbprints,
            self.peer_roster_revision,
            self.peer_roster_hash.as_deref(),
        )?;
        safe_id("nonce", &self.nonce)?;
        safe_id("jti", &self.jti)?;
        validate_signature_window(self.issued_at, self.expires_at, now_unix)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>, V2ProtocolError> {
        domain_separated_canonical(HELLO_PROOF_DOMAIN, self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedHelloProof {
    pub endpoint_key: EndpointPublicKey,
    pub claims: HelloProofClaims,
    pub signature: String,
}

impl SignedHelloProof {
    pub fn verify(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        self.endpoint_key.validate()?;
        self.claims.validate(now_unix)?;
        if self.endpoint_key.thumbprint != self.claims.holder_key_thumbprint {
            return Err(V2ProtocolError::Unsafe(
                "hello proof holder key does not match its claims",
            ));
        }
        self.endpoint_key
            .verify(&self.claims.signing_bytes()?, &self.signature)
    }
}

/// Portable claims FormLogic or a custom identity service signs for one
/// short-lived v2 WebSocket admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdmissionClaims {
    pub aud: String,
    pub app_id: String,
    pub subject_id: String,
    pub role: AdmissionRole,
    pub holder_key_thumbprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_peer_key_thumbprint: Option<String>,
    #[serde(default)]
    pub approved_peer_key_thumbprints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_roster_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_roster_hash: Option<String>,
    pub scopes: Vec<Grant>,
    pub exp: u64,
    pub jti: String,
}

impl AdmissionClaims {
    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        exact("aud", &self.aud, ADMISSION_AUDIENCE)?;
        safe_id("appId", &self.app_id)?;
        safe_id("subjectId", &self.subject_id)?;
        safe_id("holderKeyThumbprint", &self.holder_key_thumbprint)?;
        validate_peer_policy(
            self.role,
            &self.holder_key_thumbprint,
            self.expected_peer_key_thumbprint.as_deref(),
            &self.approved_peer_key_thumbprints,
            self.peer_roster_revision,
            self.peer_roster_hash.as_deref(),
        )?;
        safe_integer("exp", self.exp, 1)?;
        safe_id("jti", &self.jti)?;
        if self.exp <= now_unix {
            return Err(V2ProtocolError::Expired);
        }
        if self.scopes.len() > 16 {
            return Err(V2ProtocolError::Invalid("scopes"));
        }
        let mut unique = self.scopes.clone();
        unique.sort_by_key(|scope| *scope as u8);
        unique.dedup();
        if unique.len() != self.scopes.len() {
            return Err(V2ProtocolError::Invalid("scopes"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    Monitor,
    Consult,
    Takeover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeasePhase {
    Prepared,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MobileOfferSurface {
    InApp,
    VoiceSystemUi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingMobileOfferClaims {
    pub offer_id: String,
    pub opportunity_id: String,
    pub target_device_id: String,
    pub target_holder_key_thumbprint: String,
    pub offered_mode: LeaseMode,
    pub surface: MobileOfferSurface,
    pub app_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub required_consent_policy_id: String,
    pub required_consent_policy_version: u32,
    pub required_grants: Vec<Grant>,
    pub issued_at: u64,
    pub expires_at: u64,
    pub jti: String,
}

impl PendingMobileOfferClaims {
    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        safe_id("offerId", &self.offer_id)?;
        safe_id("opportunityId", &self.opportunity_id)?;
        safe_id("targetDeviceId", &self.target_device_id)?;
        safe_id(
            "targetHolderKeyThumbprint",
            &self.target_holder_key_thumbprint,
        )?;
        safe_id("appId", &self.app_id)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("switchboardRevision", self.switchboard_revision, 0)?;
        safe_integer("remoteRevision", self.remote_revision, 0)?;
        safe_id("requiredConsentPolicyId", &self.required_consent_policy_id)?;
        safe_integer(
            "requiredConsentPolicyVersion",
            u64::from(self.required_consent_policy_version),
            1,
        )?;
        safe_id("jti", &self.jti)?;
        validate_signature_window(self.issued_at, self.expires_at, now_unix)?;
        if self.expires_at.saturating_sub(self.issued_at) > MOBILE_OFFER_MAX_LIFETIME {
            return Err(V2ProtocolError::Expired);
        }
        if matches!(self.surface, MobileOfferSurface::VoiceSystemUi)
            && matches!(self.offered_mode, LeaseMode::Monitor)
        {
            return Err(V2ProtocolError::Unsafe(
                "voice system UI offers must be consult or takeover",
            ));
        }
        let mode_grant = match self.offered_mode {
            LeaseMode::Monitor => Grant::Monitor,
            LeaseMode::Consult => Grant::Consult,
            LeaseMode::Takeover => Grant::Takeover,
        };
        if !self.required_grants.contains(&Grant::StateRead)
            || !self.required_grants.contains(&Grant::RtcSignal)
            || !self.required_grants.contains(&mode_grant)
        {
            return Err(V2ProtocolError::Unsafe(
                "mobile offer omits a required grant",
            ));
        }
        let mut grants = self.required_grants.clone();
        grants.sort_by_key(|grant| *grant as u8);
        grants.dedup();
        if grants.len() != self.required_grants.len() || grants.len() > 16 {
            return Err(V2ProtocolError::Invalid("requiredGrants"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedPendingMobileOffer {
    pub offer: PendingMobileOfferClaims,
    pub offer_token: String,
}

impl SignedPendingMobileOffer {
    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        self.offer.validate(now_unix)?;
        bounded_text("offerToken", &self.offer_token, MAX_LEASE_TOKEN_BYTES)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTrack {
    PstnIn,
    PstnOut,
    ConsultRx,
    ConsultTx,
}

pub fn tracks_for(mode: LeaseMode, phase: LeasePhase) -> Vec<MediaTrack> {
    match (mode, phase) {
        // A monitor hears the complete cellular conversation: actual caller
        // ingress plus the exact PCM that was ultimately queued to SCO TX.
        // This still opens no Companion microphone/caller-transmit route.
        (LeaseMode::Monitor, _) => vec![MediaTrack::PstnIn, MediaTrack::PstnOut],
        // A prepared consult is receive-only.  The Companion microphone is
        // absent until Desktop has atomically entered software hold and the
        // gateway rotates the lease/owner epoch.  This is a protocol
        // boundary, not merely a UI convention.
        (LeaseMode::Consult, LeasePhase::Prepared) => vec![MediaTrack::ConsultRx],
        (LeaseMode::Consult, LeasePhase::Active) => {
            vec![MediaTrack::ConsultRx, MediaTrack::ConsultTx]
        }
        (LeaseMode::Takeover, LeasePhase::Prepared) => vec![MediaTrack::PstnIn],
        (LeaseMode::Takeover, LeasePhase::Active) => {
            vec![MediaTrack::PstnIn, MediaTrack::PstnOut]
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CarrierHoldEvidence {
    Unknown,
    Negotiated,
    Observed,
    Proven,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecondaryCallObservation {
    Unknown,
    Negotiated,
    Observed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecondaryCallPolicy {
    Normal,
    MissAndCallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecondaryCallStatus {
    Queued,
    Attempted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteCapabilities {
    pub software_hold: bool,
    pub carrier_hold_evidence: CarrierHoldEvidence,
    pub secondary_call_observation: SecondaryCallObservation,
    pub voice_consult: bool,
    pub takeover: bool,
}

impl RemoteCapabilities {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        if self.voice_consult && !self.software_hold {
            return Err(V2ProtocolError::Unsafe(
                "voice consultation requires software hold",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScreenedSecondaryCall {
    pub stable: bool,
    pub callback_eligible: bool,
    pub status: SecondaryCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_call_id: Option<String>,
}

impl ScreenedSecondaryCall {
    fn validate(&self, observation: SecondaryCallObservation) -> Result<(), V2ProtocolError> {
        if !matches!(observation, SecondaryCallObservation::Observed) {
            return Err(V2ProtocolError::Unsafe(
                "secondary-call status requires observed carrier evidence",
            ));
        }
        if let Some(waiting_call_id) = &self.waiting_call_id {
            safe_id("secondaryCall.waitingCallId", waiting_call_id)?;
            if !self.stable || !self.callback_eligible {
                return Err(V2ProtocolError::Unsafe(
                    "waitingCallId requires stable callback eligibility",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantMode {
    Observer,
    Advisor,
    Talker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantState {
    Connected,
    Prepared,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParticipantPresence {
    pub participant_id: String,
    pub mode: ParticipantMode,
    pub state: ParticipantState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
}

impl ParticipantPresence {
    fn validate(&self) -> Result<(), V2ProtocolError> {
        safe_id("participants.participantId", &self.participant_id)?;
        if let Some(subject_id) = &self.subject_id {
            safe_id("participants.subjectId", subject_id)?;
        }
        if let Some(display_label) = &self.display_label {
            bounded_text("participants.displayLabel", display_label, 120)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioLevelSource {
    Caller,
    Aokie,
    Companion,
}

/// Normalized 0..=1000 audio level reported by a real media endpoint. The
/// gateway never manufactures entries when no level was reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedAudioLevel {
    pub source: AudioLevelSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participant_id: Option<String>,
    pub level_permille: u16,
}

impl NormalizedAudioLevel {
    fn validate(&self) -> Result<(), V2ProtocolError> {
        if let Some(participant_id) = &self.participant_id {
            safe_id("audioLevels.participantId", participant_id)?;
        }
        if self.level_permille > 1_000 {
            return Err(V2ProtocolError::Invalid("audioLevels.levelPermille"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallerProjection {
    pub label: Option<String>,
    pub masked_number: Option<String>,
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

/// Operator acknowledgement for the current, versioned remote-access
/// disclosure.  Operation flags are deliberately separate: accepting live
/// captions or listen-only monitoring never silently enables takeover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteConsentPolicy {
    pub policy_id: String,
    pub policy_version: u32,
    pub enabled: bool,
    pub acknowledged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub captions_enabled: bool,
    pub assistance_enabled: bool,
    pub monitor_enabled: bool,
    pub consult_enabled: bool,
    pub takeover_enabled: bool,
}

impl RemoteConsentPolicy {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        safe_id("remoteConsent.policyId", &self.policy_id)?;
        safe_integer(
            "remoteConsent.policyVersion",
            u64::from(self.policy_version),
            1,
        )?;
        if let Some(acknowledged_at) = &self.acknowledged_at {
            bounded_text("remoteConsent.acknowledgedAt", acknowledged_at, 64)?;
        }
        if let Some(expires_at) = &self.expires_at {
            bounded_text("remoteConsent.expiresAt", expires_at, 64)?;
        }
        if self.acknowledged != self.acknowledged_at.is_some() {
            return Err(V2ProtocolError::Invalid("remoteConsent.acknowledgedAt"));
        }
        if (!self.enabled || !self.acknowledged)
            && (self.captions_enabled
                || self.assistance_enabled
                || self.monitor_enabled
                || self.consult_enabled
                || self.takeover_enabled)
        {
            return Err(V2ProtocolError::Unsafe(
                "remote capabilities require an enabled, acknowledged current policy",
            ));
        }
        Ok(())
    }

    pub fn allows(&self, mode: LeaseMode) -> bool {
        self.enabled
            && self.acknowledged
            && match mode {
                LeaseMode::Monitor => self.monitor_enabled,
                LeaseMode::Consult => self.consult_enabled,
                LeaseMode::Takeover => self.takeover_enabled,
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoritativeCallSnapshot {
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub telephony_state: TelephonyState,
    pub service_mode: ServiceMode,
    pub media_state: MediaState,
    pub remote_capabilities: RemoteCapabilities,
    pub secondary_call_policy: SecondaryCallPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_call: Option<ScreenedSecondaryCall>,
    pub remote_consent: RemoteConsentPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<CallerProjection>,
    #[serde(default)]
    pub captions: Vec<Caption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_levels: Option<Vec<NormalizedAudioLevel>>,
    pub occurred_at: String,
}

impl AuthoritativeCallSnapshot {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("switchboardRevision", self.switchboard_revision, 0)?;
        safe_integer("remoteRevision", self.remote_revision, 0)?;
        self.remote_capabilities.validate()?;
        if let Some(secondary_call) = &self.secondary_call {
            secondary_call.validate(self.remote_capabilities.secondary_call_observation)?;
            if !matches!(
                self.secondary_call_policy,
                SecondaryCallPolicy::MissAndCallback
            ) && secondary_call.callback_eligible
            {
                return Err(V2ProtocolError::Unsafe(
                    "callback eligibility requires miss_and_callback policy",
                ));
            }
        }
        self.remote_consent.validate()?;
        if self.captions.len() > MAX_CAPTIONS {
            return Err(V2ProtocolError::Invalid("captions"));
        }
        for caption in &self.captions {
            safe_id("captionId", &caption.caption_id)?;
            bounded_text("speaker", &caption.speaker, 40)?;
            bounded_text("captionText", &caption.text, MAX_CAPTION_BYTES)?;
            bounded_text("occurredAt", &caption.occurred_at, 64)?;
        }
        if let Some(audio_levels) = &self.audio_levels {
            if audio_levels.len() > MAX_AUDIO_LEVELS {
                return Err(V2ProtocolError::Invalid("audioLevels"));
            }
            for level in audio_levels {
                level.validate()?;
            }
        }
        if let Some(caller) = &self.caller {
            if let Some(label) = &caller.label {
                bounded_text("caller.label", label, 200)?;
            }
            if let Some(number) = &caller.masked_number {
                bounded_text("caller.maskedNumber", number, 40)?;
            }
        }
        bounded_text("occurredAt", &self.occurred_at, 64)?;
        if matches!(self.telephony_state, TelephonyState::Ended)
            != matches!(self.service_mode, ServiceMode::Ended)
        {
            return Err(V2ProtocolError::Unsafe(
                "telephony and service ended states disagree",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectedCallSnapshot {
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub telephony_state: TelephonyState,
    pub service_mode: ServiceMode,
    pub media_state: MediaState,
    pub remote_capabilities: RemoteCapabilities,
    pub secondary_call_policy: SecondaryCallPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_call: Option<ScreenedSecondaryCall>,
    pub remote_consent: RemoteConsentPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<CallerProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captions: Option<Vec<Caption>>,
    #[serde(default)]
    pub participants: Vec<ParticipantPresence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_levels: Option<Vec<NormalizedAudioLevel>>,
    #[serde(default)]
    pub pending_mobile_offers: Vec<SignedPendingMobileOffer>,
    pub occurred_at: String,
}

impl ProjectedCallSnapshot {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("switchboardRevision", self.switchboard_revision, 0)?;
        safe_integer("remoteRevision", self.remote_revision, 0)?;
        self.remote_capabilities.validate()?;
        self.remote_consent.validate()?;
        if let Some(secondary_call) = &self.secondary_call {
            secondary_call.validate(self.remote_capabilities.secondary_call_observation)?;
            if !matches!(
                self.secondary_call_policy,
                SecondaryCallPolicy::MissAndCallback
            ) && secondary_call.callback_eligible
            {
                return Err(V2ProtocolError::Unsafe(
                    "callback eligibility requires miss_and_callback policy",
                ));
            }
        }
        if self.participants.len() > MAX_PARTICIPANTS {
            return Err(V2ProtocolError::Invalid("participants"));
        }
        for participant in &self.participants {
            participant.validate()?;
        }
        if let Some(levels) = &self.audio_levels {
            if levels.len() > MAX_AUDIO_LEVELS {
                return Err(V2ProtocolError::Invalid("audioLevels"));
            }
            for level in levels {
                level.validate()?;
            }
        }
        if self.pending_mobile_offers.len() > MAX_PENDING_MOBILE_OFFERS {
            return Err(V2ProtocolError::Invalid("pendingMobileOffers"));
        }
        for offer in &self.pending_mobile_offers {
            // Shape and internal lifetime are checked here; the receiver uses
            // its current clock before presenting or answering the offer.
            offer.offer.validate(offer.offer.issued_at)?;
            bounded_text("offerToken", &offer.offer_token, MAX_LEASE_TOKEN_BYTES)?;
        }
        bounded_text("occurredAt", &self.occurred_at, 64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileHello {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub device_id: String,
    pub session_nonce: String,
    pub endpoint_proof: SignedHelloProof,
}

impl MobileHello {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "mobile_hello")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("deviceId", &self.device_id)?;
        safe_id("sessionNonce", &self.session_nonce)?;
        if self.endpoint_proof.claims.role != AdmissionRole::Mobile
            || self.endpoint_proof.claims.app_id != self.app_id
            || self.endpoint_proof.claims.subject_id != self.device_id
            || self.endpoint_proof.claims.session_nonce != self.session_nonce
        {
            return Err(V2ProtocolError::Unsafe(
                "mobile hello does not match its endpoint proof",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginHello {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub plugin_id: String,
    pub session_nonce: String,
    pub endpoint_proof: SignedHelloProof,
}

impl PluginHello {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "plugin_hello")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("pluginId", &self.plugin_id)?;
        safe_id("sessionNonce", &self.session_nonce)?;
        if self.endpoint_proof.claims.role != AdmissionRole::Plugin
            || self.endpoint_proof.claims.app_id != self.app_id
            || self.endpoint_proof.claims.subject_id != self.plugin_id
            || self.endpoint_proof.claims.session_nonce != self.session_nonce
        {
            return Err(V2ProtocolError::Unsafe(
                "plugin hello does not match its endpoint proof",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginIdleFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub event_id: String,
}

impl PluginIdleFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "plugin_idle")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("eventId", &self.event_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginSnapshotFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub event_id: String,
    pub snapshot: AuthoritativeCallSnapshot,
}

impl PluginSnapshotFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "plugin_snapshot")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("eventId", &self.event_id)?;
        self.snapshot.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileSnapshotFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub sequence: u64,
    pub grants: Vec<Grant>,
    pub snapshot: ProjectedCallSnapshot,
}

/// Authenticated gateway assertion that the endpoint-authoritative state is
/// currently idle. This is deliberately not a synthetic call snapshot: no
/// call identity, epoch, media authority, or control fence exists while idle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileIdleSyncFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub sequence: u64,
    pub grants: Vec<Grant>,
}

impl MobileIdleSyncFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "idle_sync")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_integer("sequence", self.sequence, 1)?;
        if self.grants.len() > 16 || !self.grants.contains(&Grant::StateRead) {
            return Err(V2ProtocolError::Invalid("grants"));
        }
        let mut unique = self.grants.clone();
        unique.sort_by_key(|grant| *grant as u8);
        unique.dedup();
        if unique.len() != self.grants.len() {
            return Err(V2ProtocolError::Invalid("grants"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseRequestFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub call_id: String,
    pub expected_call_epoch: u64,
    pub expected_owner_epoch: u64,
    pub expected_switchboard_revision: u64,
    pub expected_remote_revision: u64,
    pub mode: LeaseMode,
    pub rtc_session_id: String,
    pub accepted_offer_id: String,
    pub accepted_offer_jti: String,
}

impl LeaseRequestFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "lease_request")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("requestId", &self.request_id)?;
        idempotency_key(&self.idempotency_key)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("expectedCallEpoch", self.expected_call_epoch, 1)?;
        safe_integer("expectedOwnerEpoch", self.expected_owner_epoch, 0)?;
        safe_integer(
            "expectedSwitchboardRevision",
            self.expected_switchboard_revision,
            0,
        )?;
        safe_integer("expectedRemoteRevision", self.expected_remote_revision, 0)?;
        safe_id("rtcSessionId", &self.rtc_session_id)?;
        safe_id("acceptedOfferId", &self.accepted_offer_id)?;
        safe_id("acceptedOfferJti", &self.accepted_offer_jti)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileOfferAnswerFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub offer_id: String,
    pub offer_jti: String,
    pub offer_token: String,
    pub target_device_id: String,
    pub target_holder_key_thumbprint: String,
    pub offered_mode: LeaseMode,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
}

impl MobileOfferAnswerFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "mobile_offer_answer")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("requestId", &self.request_id)?;
        idempotency_key(&self.idempotency_key)?;
        safe_id("offerId", &self.offer_id)?;
        safe_id("offerJti", &self.offer_jti)?;
        bounded_text("offerToken", &self.offer_token, MAX_LEASE_TOKEN_BYTES)?;
        safe_id("targetDeviceId", &self.target_device_id)?;
        safe_id(
            "targetHolderKeyThumbprint",
            &self.target_holder_key_thumbprint,
        )?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseHeartbeatFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub lease_token: String,
}

impl LeaseHeartbeatFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "lease_heartbeat")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("requestId", &self.request_id)?;
        idempotency_key(&self.idempotency_key)?;
        bounded_text("leaseToken", &self.lease_token, MAX_LEASE_TOKEN_BYTES)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseRevokeFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub lease_token: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginAssistanceRequestFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub event_id: String,
    pub request_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub expires_at: u64,
}

impl PluginAssistanceRequestFrame {
    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "assistance_request")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("eventId", &self.event_id)?;
        safe_id("requestId", &self.request_id)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("switchboardRevision", self.switchboard_revision, 0)?;
        safe_integer("remoteRevision", self.remote_revision, 0)?;
        bounded_text("question", &self.question, MAX_ASSISTANCE_QUESTION_BYTES)?;
        if let Some(context) = &self.context {
            bounded_text("context", context, MAX_ASSISTANCE_CONTEXT_BYTES)?;
        }
        safe_integer("expiresAt", self.expires_at, 1)?;
        if self.expires_at <= now_unix {
            return Err(V2ProtocolError::Expired);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileAssistanceAnswerFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub answer_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub answer: String,
}

impl MobileAssistanceAnswerFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "assistance_answer")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("requestId", &self.request_id)?;
        idempotency_key(&self.idempotency_key)?;
        safe_id("answerId", &self.answer_id)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("switchboardRevision", self.switchboard_revision, 0)?;
        safe_integer("remoteRevision", self.remote_revision, 0)?;
        bounded_text("answer", &self.answer, MAX_ASSISTANCE_ANSWER_BYTES)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginAssistanceAnswerFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub device_id: String,
    pub request_id: String,
    pub answer_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub answer: String,
}

impl PluginAssistanceAnswerFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "assistance_answer")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("deviceId", &self.device_id)?;
        safe_id("requestId", &self.request_id)?;
        safe_id("answerId", &self.answer_id)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("switchboardRevision", self.switchboard_revision, 0)?;
        safe_integer("remoteRevision", self.remote_revision, 0)?;
        bounded_text("answer", &self.answer, MAX_ASSISTANCE_ANSWER_BYTES)
    }
}

impl LeaseRevokeFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "lease_revoke")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("requestId", &self.request_id)?;
        idempotency_key(&self.idempotency_key)?;
        bounded_text("leaseToken", &self.lease_token, MAX_LEASE_TOKEN_BYTES)?;
        bounded_text("reason", &self.reason, MAX_REASON_BYTES)
    }
}

/// First half of the deliberately two-step caller-ending operation.  The
/// lease token never leaves the native client; the gateway verifies it and
/// returns a short-lived, one-use confirmation nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileEndCallerChallengeRequestFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub lease_token: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub fence: u64,
}

impl MobileEndCallerChallengeRequestFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "end_caller_challenge_request")?;
        validate_end_caller_mobile_fields(
            self.schema_version,
            &self.app_id,
            &self.request_id,
            &self.idempotency_key,
            &self.lease_token,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.switchboard_revision,
            self.remote_revision,
            self.fence,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileEndCallerConfirmFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub confirmation_id: String,
    pub nonce: String,
    pub lease_token: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub fence: u64,
}

impl MobileEndCallerConfirmFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "end_caller_confirm")?;
        safe_id("confirmationId", &self.confirmation_id)?;
        safe_id("nonce", &self.nonce)?;
        validate_end_caller_mobile_fields(
            self.schema_version,
            &self.app_id,
            &self.request_id,
            &self.idempotency_key,
            &self.lease_token,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.switchboard_revision,
            self.remote_revision,
            self.fence,
        )
    }
}

fn validate_end_caller_mobile_fields(
    schema_version: u16,
    app_id: &str,
    request_id: &str,
    idempotency: &str,
    lease_token: &str,
    call_id: &str,
    call_epoch: u64,
    owner_epoch: u64,
    switchboard_revision: u64,
    remote_revision: u64,
    fence: u64,
) -> Result<(), V2ProtocolError> {
    schema(schema_version)?;
    safe_id("appId", app_id)?;
    safe_id("requestId", request_id)?;
    idempotency_key(idempotency)?;
    bounded_text("leaseToken", lease_token, MAX_LEASE_TOKEN_BYTES)?;
    safe_id("callId", call_id)?;
    safe_integer("callEpoch", call_epoch, 1)?;
    safe_integer("ownerEpoch", owner_epoch, 1)?;
    safe_integer("switchboardRevision", switchboard_revision, 0)?;
    safe_integer("remoteRevision", remote_revision, 0)?;
    safe_integer("fence", fence, 1)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndCallerChallengeFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub confirmation_id: String,
    pub nonce: String,
    pub device_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub lease_id: String,
    pub fence: u64,
    pub expires_at: u64,
}

impl EndCallerChallengeFrame {
    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "end_caller_challenge")?;
        validate_end_caller_operation_fields(
            self.schema_version,
            &self.app_id,
            &self.device_id,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.switchboard_revision,
            self.remote_revision,
            &self.lease_id,
            self.fence,
        )?;
        safe_id("requestId", &self.request_id)?;
        safe_id("confirmationId", &self.confirmation_id)?;
        safe_id("nonce", &self.nonce)?;
        safe_integer("expiresAt", self.expires_at, 1)?;
        if self.expires_at <= now_unix {
            return Err(V2ProtocolError::Expired);
        }
        Ok(())
    }
}

/// Gateway-to-plugin command.  It contains every physical and media-owner
/// fence so a queued command can never land on a later call or returned
/// Aokie session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginEndCallerExecuteFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub operation_id: String,
    pub confirmation_id: String,
    pub device_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub lease_id: String,
    pub lease_jti: String,
    pub fence: u64,
}

impl PluginEndCallerExecuteFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "end_caller_execute")?;
        validate_end_caller_operation_fields(
            self.schema_version,
            &self.app_id,
            &self.device_id,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.switchboard_revision,
            self.remote_revision,
            &self.lease_id,
            self.fence,
        )?;
        safe_id("operationId", &self.operation_id)?;
        safe_id("confirmationId", &self.confirmation_id)?;
        safe_id("leaseJti", &self.lease_jti)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginEndCallerResultFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub operation_id: String,
    pub confirmation_id: String,
    pub device_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub lease_id: String,
    pub fence: u64,
    pub outcome: EndCallerOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl PluginEndCallerResultFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "end_caller_result")?;
        validate_end_caller_operation_fields(
            self.schema_version,
            &self.app_id,
            &self.device_id,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.switchboard_revision,
            self.remote_revision,
            &self.lease_id,
            self.fence,
        )?;
        safe_id("operationId", &self.operation_id)?;
        safe_id("confirmationId", &self.confirmation_id)?;
        match self.outcome {
            EndCallerOutcome::Completed if self.code.is_none() && self.message.is_none() => Ok(()),
            EndCallerOutcome::Failed if self.code.is_some() && self.message.is_some() => {
                safe_id("code", self.code.as_deref().expect("checked"))?;
                bounded_text(
                    "message",
                    self.message.as_deref().expect("checked"),
                    MAX_REASON_BYTES,
                )
            }
            _ => Err(V2ProtocolError::Invalid("outcome")),
        }
    }
}

fn validate_end_caller_operation_fields(
    schema_version: u16,
    app_id: &str,
    device_id: &str,
    call_id: &str,
    call_epoch: u64,
    owner_epoch: u64,
    switchboard_revision: u64,
    remote_revision: u64,
    lease_id: &str,
    fence: u64,
) -> Result<(), V2ProtocolError> {
    schema(schema_version)?;
    safe_id("appId", app_id)?;
    safe_id("deviceId", device_id)?;
    safe_id("callId", call_id)?;
    safe_integer("callEpoch", call_epoch, 1)?;
    safe_integer("ownerEpoch", owner_epoch, 1)?;
    safe_integer("switchboardRevision", switchboard_revision, 0)?;
    safe_integer("remoteRevision", remote_revision, 0)?;
    safe_id("leaseId", lease_id)?;
    safe_integer("fence", fence, 1)
}

/// Endpoint-authenticated SDP binding. A fresh binding is required for each
/// offer/answer revision and therefore for every ICE restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointBindingClaims {
    pub app_id: String,
    pub plugin_id: String,
    pub device_id: String,
    pub rtc_session_id: String,
    pub endpoint_session_nonce: String,
    pub lease_jti: String,
    pub endpoint_role: AdmissionRole,
    pub holder_key_thumbprint: String,
    pub peer_key_thumbprint: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub fence: u64,
    pub sdp_revision: u64,
    pub transport_generation: u64,
    pub dtls_fingerprint: String,
    pub sdp_sha256: String,
    pub nonce: String,
    pub jti: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

impl EndpointBindingClaims {
    fn validate_shape(&self) -> Result<(), V2ProtocolError> {
        validate_rtc_identity(
            &self.app_id,
            &self.plugin_id,
            &self.device_id,
            &self.rtc_session_id,
            &self.endpoint_session_nonce,
            &self.lease_jti,
            &self.holder_key_thumbprint,
            &self.peer_key_thumbprint,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.fence,
            self.sdp_revision,
            self.transport_generation,
            &self.nonce,
            &self.jti,
        )?;
        bounded_text("dtlsFingerprint", &self.dtls_fingerprint, 200)?;
        safe_id("sdpSha256", &self.sdp_sha256)?;
        safe_integer("issuedAt", self.issued_at, 1)?;
        safe_integer("expiresAt", self.expires_at, 1)
    }

    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        self.validate_shape()?;
        validate_signature_window(self.issued_at, self.expires_at, now_unix)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>, V2ProtocolError> {
        domain_separated_canonical(ENDPOINT_BINDING_DOMAIN, self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedEndpointBinding {
    pub endpoint_key: EndpointPublicKey,
    pub claims: EndpointBindingClaims,
    pub signature: String,
}

impl SignedEndpointBinding {
    pub fn validate_shape(&self) -> Result<(), V2ProtocolError> {
        self.endpoint_key.validate()?;
        self.claims.validate_shape()?;
        validate_encoded_signature(&self.signature)?;
        if self.endpoint_key.thumbprint != self.claims.holder_key_thumbprint {
            return Err(V2ProtocolError::Unsafe(
                "SDP binding holder key does not match its claims",
            ));
        }
        Ok(())
    }

    pub fn verify_for_sdp(&self, sdp: &str, now_unix: u64) -> Result<(), V2ProtocolError> {
        self.validate_shape()?;
        self.claims.validate(now_unix)?;
        if self.claims.sdp_sha256 != sdp_sha256(sdp)
            || self.claims.dtls_fingerprint != sdp_dtls_fingerprint(sdp)?
        {
            return Err(V2ProtocolError::Unsafe(
                "SDP content does not match its endpoint binding",
            ));
        }
        self.endpoint_key
            .verify(&self.claims.signing_bytes()?, &self.signature)
    }
}

/// Endpoint-authenticated candidate (or end-of-candidates marker). The
/// envelope repeats every routing and epoch field so candidates cannot be
/// transplanted between calls, revisions, peers, or ICE generations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrickleCandidateClaims {
    pub app_id: String,
    pub plugin_id: String,
    pub device_id: String,
    pub rtc_session_id: String,
    pub endpoint_session_nonce: String,
    pub lease_jti: String,
    pub endpoint_role: AdmissionRole,
    pub holder_key_thumbprint: String,
    pub peer_key_thumbprint: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub fence: u64,
    pub sdp_revision: u64,
    pub transport_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_mid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdp_m_line_index: Option<u16>,
    pub end_of_candidates: bool,
    pub nonce: String,
    pub jti: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

impl TrickleCandidateClaims {
    fn validate_shape(&self) -> Result<(), V2ProtocolError> {
        validate_rtc_identity(
            &self.app_id,
            &self.plugin_id,
            &self.device_id,
            &self.rtc_session_id,
            &self.endpoint_session_nonce,
            &self.lease_jti,
            &self.holder_key_thumbprint,
            &self.peer_key_thumbprint,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.fence,
            self.sdp_revision,
            self.transport_generation,
            &self.nonce,
            &self.jti,
        )?;
        match (
            self.end_of_candidates,
            self.candidate.as_deref(),
            self.sdp_mid.as_deref(),
            self.sdp_m_line_index,
        ) {
            (false, Some(candidate), Some(mid), Some(_)) => {
                bounded_text("candidate", candidate, MAX_ICE_BYTES)?;
                bounded_text("sdpMid", mid, 256)?;
            }
            (true, None, None, None) => {}
            _ => return Err(V2ProtocolError::Invalid("candidateEnvelope")),
        }
        safe_integer("issuedAt", self.issued_at, 1)?;
        safe_integer("expiresAt", self.expires_at, 1)
    }

    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        self.validate_shape()?;
        validate_signature_window(self.issued_at, self.expires_at, now_unix)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>, V2ProtocolError> {
        domain_separated_canonical(TRICKLE_CANDIDATE_DOMAIN, self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedTrickleCandidateEnvelope {
    pub endpoint_key: EndpointPublicKey,
    pub claims: TrickleCandidateClaims,
    pub signature: String,
}

impl SignedTrickleCandidateEnvelope {
    pub fn validate_shape(&self) -> Result<(), V2ProtocolError> {
        self.endpoint_key.validate()?;
        self.claims.validate_shape()?;
        validate_encoded_signature(&self.signature)?;
        if self.endpoint_key.thumbprint != self.claims.holder_key_thumbprint {
            return Err(V2ProtocolError::Unsafe(
                "candidate holder key does not match its claims",
            ));
        }
        Ok(())
    }

    pub fn verify_for_candidate(
        &self,
        candidate: Option<&str>,
        sdp_mid: Option<&str>,
        sdp_m_line_index: Option<u16>,
        end_of_candidates: bool,
        now_unix: u64,
    ) -> Result<(), V2ProtocolError> {
        self.validate_shape()?;
        self.claims.validate(now_unix)?;
        if self.claims.candidate.as_deref() != candidate
            || self.claims.sdp_mid.as_deref() != sdp_mid
            || self.claims.sdp_m_line_index != sdp_m_line_index
            || self.claims.end_of_candidates != end_of_candidates
        {
            return Err(V2ProtocolError::Unsafe(
                "candidate content does not match its signed envelope",
            ));
        }
        self.endpoint_key
            .verify(&self.claims.signing_bytes()?, &self.signature)
    }
}

pub fn sdp_sha256(sdp: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(sdp.as_bytes()))
}

pub fn sdp_dtls_fingerprint(sdp: &str) -> Result<String, V2ProtocolError> {
    let mut found: Option<String> = None;
    for line in sdp.lines().map(str::trim) {
        let Some(value) = line.strip_prefix("a=fingerprint:") else {
            continue;
        };
        let (algorithm, digest) = value
            .split_once(char::is_whitespace)
            .ok_or(V2ProtocolError::Invalid("sdpFingerprint"))?;
        if !algorithm.eq_ignore_ascii_case("sha-256")
            || digest.len() != 95
            || digest.split(':').count() != 32
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b':')
        {
            return Err(V2ProtocolError::Invalid("sdpFingerprint"));
        }
        let normalized = format!("sha-256 {}", digest.to_ascii_uppercase());
        if found.as_ref().is_some_and(|current| current != &normalized) {
            return Err(V2ProtocolError::Unsafe(
                "SDP contains conflicting DTLS fingerprints",
            ));
        }
        found = Some(normalized);
    }
    found.ok_or(V2ProtocolError::Invalid("sdpFingerprint"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RtcSignal {
    Offer {
        sdp: String,
        binding: SignedEndpointBinding,
    },
    Answer {
        sdp: String,
        binding: SignedEndpointBinding,
    },
    Ice {
        candidate: String,
        #[serde(rename = "sdpMid", default, skip_serializing_if = "Option::is_none")]
        sdp_mid: Option<String>,
        #[serde(
            rename = "sdpMLineIndex",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        sdp_m_line_index: Option<u16>,
        envelope: SignedTrickleCandidateEnvelope,
    },
    IceComplete {
        envelope: SignedTrickleCandidateEnvelope,
    },
    Close {
        reason: String,
    },
}

impl RtcSignal {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        match self {
            Self::Offer { sdp, binding } | Self::Answer { sdp, binding } => {
                bounded_text("sdp", sdp, MAX_SDP_BYTES)?;
                binding.validate_shape()
            }
            Self::Ice {
                candidate,
                sdp_mid,
                sdp_m_line_index,
                envelope,
            } => {
                bounded_text("candidate", candidate, MAX_ICE_BYTES)?;
                if let Some(mid) = sdp_mid {
                    bounded_text("sdpMid", mid, 256)?;
                }
                envelope.validate_shape()?;
                if envelope.claims.candidate.as_deref() != Some(candidate)
                    || envelope.claims.sdp_mid.as_deref() != sdp_mid.as_deref()
                    || envelope.claims.sdp_m_line_index != *sdp_m_line_index
                    || envelope.claims.end_of_candidates
                {
                    return Err(V2ProtocolError::Unsafe(
                        "candidate signal and signed envelope disagree",
                    ));
                }
                Ok(())
            }
            Self::IceComplete { envelope } => {
                envelope.validate_shape()?;
                if !envelope.claims.end_of_candidates {
                    return Err(V2ProtocolError::Unsafe(
                        "ICE completion and signed envelope disagree",
                    ));
                }
                Ok(())
            }
            Self::Close { reason } => bounded_text("reason", reason, MAX_REASON_BYTES),
        }
    }

    pub fn verify_endpoint_authentication(
        &self,
        now_unix: u64,
    ) -> Result<Option<RtcAuthenticationRef<'_>>, V2ProtocolError> {
        match self {
            Self::Offer { sdp, binding } | Self::Answer { sdp, binding } => {
                binding.verify_for_sdp(sdp, now_unix)?;
                Ok(Some(RtcAuthenticationRef::Binding(&binding.claims)))
            }
            Self::Ice {
                candidate,
                sdp_mid,
                sdp_m_line_index,
                envelope,
            } => {
                envelope.verify_for_candidate(
                    Some(candidate),
                    sdp_mid.as_deref(),
                    *sdp_m_line_index,
                    false,
                    now_unix,
                )?;
                Ok(Some(RtcAuthenticationRef::Candidate(&envelope.claims)))
            }
            Self::IceComplete { envelope } => {
                envelope.verify_for_candidate(None, None, None, true, now_unix)?;
                Ok(Some(RtcAuthenticationRef::Candidate(&envelope.claims)))
            }
            Self::Close { .. } => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum RtcAuthenticationRef<'a> {
    Binding(&'a EndpointBindingClaims),
    Candidate(&'a TrickleCandidateClaims),
}

impl<'a> RtcAuthenticationRef<'a> {
    pub fn endpoint_role(self) -> AdmissionRole {
        match self {
            Self::Binding(claims) => claims.endpoint_role,
            Self::Candidate(claims) => claims.endpoint_role,
        }
    }

    pub fn endpoint_session_nonce(self) -> &'a str {
        match self {
            Self::Binding(claims) => &claims.endpoint_session_nonce,
            Self::Candidate(claims) => &claims.endpoint_session_nonce,
        }
    }

    pub fn holder_key_thumbprint(self) -> &'a str {
        match self {
            Self::Binding(claims) => &claims.holder_key_thumbprint,
            Self::Candidate(claims) => &claims.holder_key_thumbprint,
        }
    }

    pub fn peer_key_thumbprint(self) -> &'a str {
        match self {
            Self::Binding(claims) => &claims.peer_key_thumbprint,
            Self::Candidate(claims) => &claims.peer_key_thumbprint,
        }
    }

    pub fn jti(self) -> &'a str {
        match self {
            Self::Binding(claims) => &claims.jti,
            Self::Candidate(claims) => &claims.jti,
        }
    }

    pub fn expires_at(self) -> u64 {
        match self {
            Self::Binding(claims) => claims.expires_at,
            Self::Candidate(claims) => claims.expires_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MobileRtcSignalFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub signal_id: String,
    pub plugin_id: String,
    pub device_id: String,
    pub lease_token: String,
    pub lease_jti: String,
    pub rtc_session_id: String,
    pub sdp_revision: u64,
    pub transport_generation: u64,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub fence: u64,
    pub signal: RtcSignal,
}

impl MobileRtcSignalFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "rtc_signal")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("signalId", &self.signal_id)?;
        safe_id("pluginId", &self.plugin_id)?;
        safe_id("deviceId", &self.device_id)?;
        bounded_text("leaseToken", &self.lease_token, MAX_LEASE_TOKEN_BYTES)?;
        safe_id("leaseJti", &self.lease_jti)?;
        safe_id("rtcSessionId", &self.rtc_session_id)?;
        safe_integer("sdpRevision", self.sdp_revision, 1)?;
        safe_integer("transportGeneration", self.transport_generation, 1)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("fence", self.fence, 0)?;
        self.signal.validate()?;
        validate_signal_route(
            &self.signal,
            AdmissionRole::Mobile,
            &self.app_id,
            &self.plugin_id,
            &self.device_id,
            &self.lease_jti,
            &self.rtc_session_id,
            self.sdp_revision,
            self.transport_generation,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.fence,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginRtcSignalFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub signal_id: String,
    pub plugin_id: String,
    pub device_id: String,
    pub lease_jti: String,
    pub rtc_session_id: String,
    pub sdp_revision: u64,
    pub transport_generation: u64,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub fence: u64,
    pub signal: RtcSignal,
}

impl PluginRtcSignalFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "rtc_signal")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("signalId", &self.signal_id)?;
        safe_id("pluginId", &self.plugin_id)?;
        safe_id("deviceId", &self.device_id)?;
        safe_id("leaseJti", &self.lease_jti)?;
        safe_id("rtcSessionId", &self.rtc_session_id)?;
        safe_integer("sdpRevision", self.sdp_revision, 1)?;
        safe_integer("transportGeneration", self.transport_generation, 1)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("fence", self.fence, 0)?;
        self.signal.validate()?;
        validate_signal_route(
            &self.signal,
            AdmissionRole::Plugin,
            &self.app_id,
            &self.plugin_id,
            &self.device_id,
            &self.lease_jti,
            &self.rtc_session_id,
            self.sdp_revision,
            self.transport_generation,
            &self.call_id,
            self.call_epoch,
            self.owner_epoch,
            self.fence,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginClaimDecisionFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub request_id: String,
    pub device_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub fence: u64,
    pub accepted: bool,
    pub media_ready: bool,
    pub confirmed_owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PluginClaimDecisionFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "claim_decision")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("requestId", &self.request_id)?;
        safe_id("deviceId", &self.device_id)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("fence", self.fence, 0)?;
        safe_integer("confirmedOwnerEpoch", self.confirmed_owner_epoch, 0)?;
        safe_integer("switchboardRevision", self.switchboard_revision, 0)?;
        safe_integer("remoteRevision", self.remote_revision, 0)?;
        if let Some(reason) = &self.reason {
            bounded_text("reason", reason, MAX_REASON_BYTES)?;
        }
        if self.accepted && (!self.media_ready || self.reason.is_some()) {
            return Err(V2ProtocolError::Invalid("reason"));
        }
        if !self.accepted && (self.media_ready || self.reason.is_none()) {
            return Err(V2ProtocolError::Invalid("reason"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginLeaseRevokeFrame {
    pub kind: String,
    pub schema_version: u16,
    pub app_id: String,
    pub device_id: String,
    pub lease_id: String,
    pub lease_jti: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub fence: u64,
    pub reason: String,
}

impl PluginLeaseRevokeFrame {
    pub fn validate(&self) -> Result<(), V2ProtocolError> {
        exact("kind", &self.kind, "plugin_lease_revoke")?;
        schema(self.schema_version)?;
        safe_id("appId", &self.app_id)?;
        safe_id("deviceId", &self.device_id)?;
        safe_id("leaseId", &self.lease_id)?;
        safe_id("leaseJti", &self.lease_jti)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("fence", self.fence, 0)?;
        bounded_text("reason", &self.reason, MAX_REASON_BYTES)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseClaims {
    pub aud: String,
    pub app_id: String,
    pub plugin_id: String,
    pub device_id: String,
    pub plugin_key_thumbprint: String,
    pub mobile_key_thumbprint: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub mode: LeaseMode,
    pub phase: LeasePhase,
    pub tracks: Vec<MediaTrack>,
    pub expires_at: u64,
    pub lease_id: String,
    pub jti: String,
    pub fence: u64,
    pub session_nonce: String,
    pub rtc_session_id: String,
}

impl LeaseClaims {
    pub fn validate(&self, now_unix: u64) -> Result<(), V2ProtocolError> {
        exact("aud", &self.aud, LEASE_AUDIENCE)?;
        safe_id("appId", &self.app_id)?;
        safe_id("pluginId", &self.plugin_id)?;
        safe_id("deviceId", &self.device_id)?;
        safe_id("pluginKeyThumbprint", &self.plugin_key_thumbprint)?;
        safe_id("mobileKeyThumbprint", &self.mobile_key_thumbprint)?;
        safe_id("callId", &self.call_id)?;
        safe_integer("callEpoch", self.call_epoch, 1)?;
        safe_integer("ownerEpoch", self.owner_epoch, 0)?;
        safe_integer("expiresAt", self.expires_at, 1)?;
        safe_id("leaseId", &self.lease_id)?;
        safe_id("jti", &self.jti)?;
        safe_integer("fence", self.fence, 0)?;
        safe_id("sessionNonce", &self.session_nonce)?;
        safe_id("rtcSessionId", &self.rtc_session_id)?;
        if self.expires_at <= now_unix {
            return Err(V2ProtocolError::Expired);
        }
        if self.tracks != tracks_for(self.mode, self.phase) {
            return Err(V2ProtocolError::Unsafe("lease track matrix is invalid"));
        }
        if matches!(self.mode, LeaseMode::Takeover) && self.fence == 0 {
            return Err(V2ProtocolError::Unsafe(
                "talk-capable leases require a positive fence",
            ));
        }
        if !matches!(self.mode, LeaseMode::Takeover) && self.fence != 0 {
            return Err(V2ProtocolError::Unsafe(
                "non-caller-bound leases cannot carry a talk fence",
            ));
        }
        if matches!(self.mode, LeaseMode::Monitor) && !matches!(self.phase, LeasePhase::Active) {
            return Err(V2ProtocolError::Unsafe(
                "monitor leases are immediately active",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MobileInbound {
    Hello(MobileHello),
    OfferAnswer(MobileOfferAnswerFrame),
    LeaseRequest(LeaseRequestFrame),
    LeaseHeartbeat(LeaseHeartbeatFrame),
    LeaseRevoke(LeaseRevokeFrame),
    RtcSignal(MobileRtcSignalFrame),
    AssistanceAnswer(MobileAssistanceAnswerFrame),
    EndCallerChallengeRequest(MobileEndCallerChallengeRequestFrame),
    EndCallerConfirm(MobileEndCallerConfirmFrame),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginInbound {
    Hello(PluginHello),
    Idle(PluginIdleFrame),
    Snapshot(PluginSnapshotFrame),
    ClaimDecision(PluginClaimDecisionFrame),
    RtcSignal(PluginRtcSignalFrame),
    LeaseRevoke(PluginLeaseRevokeFrame),
    AssistanceRequest(PluginAssistanceRequestFrame),
    EndCallerResult(PluginEndCallerResultFrame),
}

pub fn parse_mobile_frame(encoded: &str) -> Result<MobileInbound, V2ProtocolError> {
    let value: Value = serde_json::from_str(encoded).map_err(|_| V2ProtocolError::Malformed)?;
    match kind(&value)? {
        "mobile_hello" => parse(value, MobileHello::validate).map(MobileInbound::Hello),
        "mobile_offer_answer" => {
            parse(value, MobileOfferAnswerFrame::validate).map(MobileInbound::OfferAnswer)
        }
        "lease_request" => {
            parse(value, LeaseRequestFrame::validate).map(MobileInbound::LeaseRequest)
        }
        "lease_heartbeat" => {
            parse(value, LeaseHeartbeatFrame::validate).map(MobileInbound::LeaseHeartbeat)
        }
        "lease_revoke" => parse(value, LeaseRevokeFrame::validate).map(MobileInbound::LeaseRevoke),
        "rtc_signal" => parse(value, MobileRtcSignalFrame::validate).map(MobileInbound::RtcSignal),
        "assistance_answer" => {
            parse(value, MobileAssistanceAnswerFrame::validate).map(MobileInbound::AssistanceAnswer)
        }
        "end_caller_challenge_request" => {
            parse(value, MobileEndCallerChallengeRequestFrame::validate)
                .map(MobileInbound::EndCallerChallengeRequest)
        }
        "end_caller_confirm" => {
            parse(value, MobileEndCallerConfirmFrame::validate).map(MobileInbound::EndCallerConfirm)
        }
        _ => Err(V2ProtocolError::WrongDirection),
    }
}

pub fn parse_plugin_frame(encoded: &str) -> Result<PluginInbound, V2ProtocolError> {
    let value: Value = serde_json::from_str(encoded).map_err(|_| V2ProtocolError::Malformed)?;
    match kind(&value)? {
        "plugin_hello" => parse(value, PluginHello::validate).map(PluginInbound::Hello),
        "plugin_idle" => parse(value, PluginIdleFrame::validate).map(PluginInbound::Idle),
        "plugin_snapshot" => {
            parse(value, PluginSnapshotFrame::validate).map(PluginInbound::Snapshot)
        }
        "claim_decision" => {
            parse(value, PluginClaimDecisionFrame::validate).map(PluginInbound::ClaimDecision)
        }
        "rtc_signal" => parse(value, PluginRtcSignalFrame::validate).map(PluginInbound::RtcSignal),
        "plugin_lease_revoke" => {
            parse(value, PluginLeaseRevokeFrame::validate).map(PluginInbound::LeaseRevoke)
        }
        "assistance_request" => {
            // Direction parsing validates shape; the receiving endpoint uses
            // its own trusted clock for freshness and maximum-lifetime gates.
            parse(value, |frame: &PluginAssistanceRequestFrame| {
                frame.validate(0)
            })
            .map(PluginInbound::AssistanceRequest)
        }
        "end_caller_result" => {
            parse(value, PluginEndCallerResultFrame::validate).map(PluginInbound::EndCallerResult)
        }
        _ => Err(V2ProtocolError::WrongDirection),
    }
}

fn parse<T, F>(value: Value, validate: F) -> Result<T, V2ProtocolError>
where
    T: for<'de> Deserialize<'de>,
    F: FnOnce(&T) -> Result<(), V2ProtocolError>,
{
    let frame = serde_json::from_value(value).map_err(|_| V2ProtocolError::Malformed)?;
    validate(&frame)?;
    Ok(frame)
}

fn kind(value: &Value) -> Result<&str, V2ProtocolError> {
    value
        .as_object()
        .and_then(|object| object.get("kind"))
        .and_then(Value::as_str)
        .ok_or(V2ProtocolError::Malformed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V2ProtocolError {
    Malformed,
    WrongDirection,
    Invalid(&'static str),
    Unsafe(&'static str),
    Expired,
}

impl std::fmt::Display for V2ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("malformed frame"),
            Self::WrongDirection => formatter.write_str("frame is not valid in this direction"),
            Self::Invalid(field) => write!(formatter, "invalid {field}"),
            Self::Unsafe(message) => formatter.write_str(message),
            Self::Expired => formatter.write_str("lease expired"),
        }
    }
}

impl std::error::Error for V2ProtocolError {}

fn schema(value: u16) -> Result<(), V2ProtocolError> {
    if value == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(V2ProtocolError::Invalid("schemaVersion"))
    }
}

fn exact(field: &'static str, value: &str, expected: &str) -> Result<(), V2ProtocolError> {
    if value == expected {
        Ok(())
    } else {
        Err(V2ProtocolError::Invalid(field))
    }
}

fn safe_integer(field: &'static str, value: u64, minimum: u64) -> Result<(), V2ProtocolError> {
    if value >= minimum && value <= MAX_SAFE_INTEGER {
        Ok(())
    } else {
        Err(V2ProtocolError::Invalid(field))
    }
}

fn safe_id(field: &'static str, value: &str) -> Result<(), V2ProtocolError> {
    if !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(V2ProtocolError::Invalid(field))
    }
}

fn idempotency_key(value: &str) -> Result<(), V2ProtocolError> {
    if !value.is_empty()
        && value.len() <= MAX_IDEMPOTENCY_BYTES
        && !value.bytes().any(|byte| byte.is_ascii_control())
    {
        Ok(())
    } else {
        Err(V2ProtocolError::Invalid("idempotencyKey"))
    }
}

fn bounded_text(field: &'static str, value: &str, maximum: usize) -> Result<(), V2ProtocolError> {
    if !value.is_empty() && value.len() <= maximum && !value.contains('\0') {
        Ok(())
    } else {
        Err(V2ProtocolError::Invalid(field))
    }
}

pub fn peer_roster_hash(revision: u64, thumbprints: &[String]) -> String {
    let mut sorted = thumbprints.to_vec();
    sorted.sort();
    let payload = serde_json::json!({
        "approvedPeerKeyThumbprints": sorted,
        "peerRosterRevision": revision,
    });
    let canonical = canonical_json_value(&payload).expect("bounded roster JSON is canonicalizable");
    let mut message = b"aokie/v2/peer-roster\0".to_vec();
    message.extend_from_slice(&canonical);
    URL_SAFE_NO_PAD.encode(Sha256::digest(message))
}

fn validate_peer_policy(
    role: AdmissionRole,
    holder: &str,
    expected_peer: Option<&str>,
    approved_peers: &[String],
    roster_revision: Option<u64>,
    roster_hash: Option<&str>,
) -> Result<(), V2ProtocolError> {
    match role {
        AdmissionRole::Mobile => {
            let expected =
                expected_peer.ok_or(V2ProtocolError::Invalid("expectedPeerKeyThumbprint"))?;
            safe_id("expectedPeerKeyThumbprint", expected)?;
            if expected == holder {
                return Err(V2ProtocolError::Invalid("expectedPeerKeyThumbprint"));
            }
            if !approved_peers.is_empty() || roster_revision.is_some() || roster_hash.is_some() {
                return Err(V2ProtocolError::Invalid("approvedPeerKeyThumbprints"));
            }
        }
        AdmissionRole::Plugin => {
            if expected_peer.is_some()
                || approved_peers.is_empty()
                || approved_peers.len() > MAX_APPROVED_PEER_KEYS
            {
                return Err(V2ProtocolError::Invalid("approvedPeerKeyThumbprints"));
            }
            for thumbprint in approved_peers {
                safe_id("approvedPeerKeyThumbprints", thumbprint)?;
            }
            if approved_peers.iter().any(|thumbprint| thumbprint == holder) {
                return Err(V2ProtocolError::Invalid("approvedPeerKeyThumbprints"));
            }
            if approved_peers
                .windows(2)
                .any(|pair| pair[0].as_str() >= pair[1].as_str())
            {
                return Err(V2ProtocolError::Invalid("approvedPeerKeyThumbprints"));
            }
            let revision = roster_revision.ok_or(V2ProtocolError::Invalid("peerRosterRevision"))?;
            safe_integer("peerRosterRevision", revision, 1)?;
            let supplied_hash = roster_hash.ok_or(V2ProtocolError::Invalid("peerRosterHash"))?;
            safe_id("peerRosterHash", supplied_hash)?;
            if supplied_hash != peer_roster_hash(revision, approved_peers) {
                return Err(V2ProtocolError::Invalid("peerRosterHash"));
            }
        }
    }
    Ok(())
}

fn validate_signature_window(
    issued_at: u64,
    expires_at: u64,
    now_unix: u64,
) -> Result<(), V2ProtocolError> {
    safe_integer("issuedAt", issued_at, 1)?;
    safe_integer("expiresAt", expires_at, 1)?;
    if expires_at <= issued_at
        || expires_at.saturating_sub(issued_at) > ENDPOINT_PROOF_MAX_LIFETIME
        || issued_at > now_unix.saturating_add(ENDPOINT_PROOF_CLOCK_SKEW)
        || expires_at <= now_unix
    {
        return Err(V2ProtocolError::Expired);
    }
    Ok(())
}

fn validate_encoded_signature(signature: &str) -> Result<(), V2ProtocolError> {
    let decoded = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| V2ProtocolError::Invalid("signature"))?;
    if decoded.len() == ENDPOINT_SIGNATURE_BYTES {
        Ok(())
    } else {
        Err(V2ProtocolError::Invalid("signature"))
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_rtc_identity(
    app_id: &str,
    plugin_id: &str,
    device_id: &str,
    rtc_session_id: &str,
    endpoint_session_nonce: &str,
    lease_jti: &str,
    holder_key_thumbprint: &str,
    peer_key_thumbprint: &str,
    call_id: &str,
    call_epoch: u64,
    owner_epoch: u64,
    fence: u64,
    sdp_revision: u64,
    transport_generation: u64,
    nonce: &str,
    jti: &str,
) -> Result<(), V2ProtocolError> {
    safe_id("appId", app_id)?;
    safe_id("pluginId", plugin_id)?;
    safe_id("deviceId", device_id)?;
    safe_id("rtcSessionId", rtc_session_id)?;
    safe_id("endpointSessionNonce", endpoint_session_nonce)?;
    safe_id("leaseJti", lease_jti)?;
    safe_id("holderKeyThumbprint", holder_key_thumbprint)?;
    safe_id("peerKeyThumbprint", peer_key_thumbprint)?;
    safe_id("callId", call_id)?;
    safe_integer("callEpoch", call_epoch, 1)?;
    safe_integer("ownerEpoch", owner_epoch, 0)?;
    safe_integer("fence", fence, 0)?;
    safe_integer("sdpRevision", sdp_revision, 1)?;
    safe_integer("transportGeneration", transport_generation, 1)?;
    safe_id("nonce", nonce)?;
    safe_id("jti", jti)
}

#[allow(clippy::too_many_arguments)]
fn validate_signal_route(
    signal: &RtcSignal,
    expected_role: AdmissionRole,
    app_id: &str,
    plugin_id: &str,
    device_id: &str,
    lease_jti: &str,
    rtc_session_id: &str,
    sdp_revision: u64,
    transport_generation: u64,
    call_id: &str,
    call_epoch: u64,
    owner_epoch: u64,
    fence: u64,
) -> Result<(), V2ProtocolError> {
    let route = match signal {
        RtcSignal::Offer { binding, .. } | RtcSignal::Answer { binding, .. } => Some((
            binding.claims.endpoint_role,
            &binding.claims.app_id,
            &binding.claims.plugin_id,
            &binding.claims.device_id,
            &binding.claims.lease_jti,
            &binding.claims.rtc_session_id,
            binding.claims.sdp_revision,
            binding.claims.transport_generation,
            &binding.claims.call_id,
            binding.claims.call_epoch,
            binding.claims.owner_epoch,
            binding.claims.fence,
        )),
        RtcSignal::Ice { envelope, .. } | RtcSignal::IceComplete { envelope } => Some((
            envelope.claims.endpoint_role,
            &envelope.claims.app_id,
            &envelope.claims.plugin_id,
            &envelope.claims.device_id,
            &envelope.claims.lease_jti,
            &envelope.claims.rtc_session_id,
            envelope.claims.sdp_revision,
            envelope.claims.transport_generation,
            &envelope.claims.call_id,
            envelope.claims.call_epoch,
            envelope.claims.owner_epoch,
            envelope.claims.fence,
        )),
        RtcSignal::Close { .. } => None,
    };
    let Some(route) = route else { return Ok(()) };
    if route.0 != expected_role
        || route.1 != app_id
        || route.2 != plugin_id
        || route.3 != device_id
        || route.4 != lease_jti
        || route.5 != rtc_session_id
        || route.6 != sdp_revision
        || route.7 != transport_generation
        || route.8 != call_id
        || route.9 != call_epoch
        || route.10 != owner_epoch
        || route.11 != fence
    {
        return Err(V2ProtocolError::Unsafe(
            "RTC signal route does not match its endpoint authentication",
        ));
    }
    Ok(())
}

fn domain_separated_canonical<T: Serialize>(
    domain: &str,
    payload: &T,
) -> Result<Vec<u8>, V2ProtocolError> {
    let value =
        serde_json::to_value(payload).map_err(|_| V2ProtocolError::Invalid("signedPayload"))?;
    let canonical = canonical_json_value(&value)?;
    let mut message = Vec::with_capacity(domain.len() + canonical.len() + 1);
    message.extend_from_slice(domain.as_bytes());
    message.push(0);
    message.extend_from_slice(&canonical);
    Ok(message)
}

/// Deterministic UTF-8 JSON used by all v2 endpoint signatures. Objects are
/// recursively sorted by Unicode key, strings use JSON escaping, arrays keep
/// their declared order, and floating-point numbers are rejected.
pub fn canonical_json_value(value: &Value) -> Result<Vec<u8>, V2ProtocolError> {
    fn write(value: &Value, out: &mut Vec<u8>) -> Result<(), V2ProtocolError> {
        match value {
            Value::Null => out.extend_from_slice(b"null"),
            Value::Bool(value) => out.extend_from_slice(if *value { b"true" } else { b"false" }),
            Value::Number(number) if number.is_i64() || number.is_u64() => {
                out.extend_from_slice(number.to_string().as_bytes())
            }
            Value::Number(_) => return Err(V2ProtocolError::Invalid("signedPayload")),
            Value::String(value) => out.extend_from_slice(
                serde_json::to_string(value)
                    .map_err(|_| V2ProtocolError::Invalid("signedPayload"))?
                    .as_bytes(),
            ),
            Value::Array(values) => {
                out.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        out.push(b',');
                    }
                    write(value, out)?;
                }
                out.push(b']');
            }
            Value::Object(values) => {
                out.push(b'{');
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort();
                for (index, key) in keys.into_iter().enumerate() {
                    if index != 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(
                        serde_json::to_string(key)
                            .map_err(|_| V2ProtocolError::Invalid("signedPayload"))?
                            .as_bytes(),
                    );
                    out.push(b':');
                    write(&values[key], out)?;
                }
                out.push(b'}');
            }
        }
        Ok(())
    }

    let mut encoded = Vec::new();
    write(value, &mut encoded)?;
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use serde_json::json;

    fn test_signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn test_endpoint_key(seed: u8) -> EndpointPublicKey {
        EndpointPublicKey::from_ed25519_bytes(&test_signing_key(seed).verifying_key().to_bytes())
    }

    fn fake_hello_proof(role: AdmissionRole, subject_id: &str) -> SignedHelloProof {
        let endpoint_key = test_endpoint_key(7);
        SignedHelloProof {
            claims: HelloProofClaims {
                app_id: "app_a".into(),
                subject_id: subject_id.into(),
                role,
                connection_id: "connection_a".into(),
                challenge_nonce: "challenge_a".into(),
                admission_jti: "admission_a".into(),
                session_nonce: "session_a".into(),
                holder_key_thumbprint: endpoint_key.thumbprint.clone(),
                expected_peer_key_thumbprint: Some("peer_thumbprint_a".into()),
                approved_peer_key_thumbprints: vec![],
                peer_roster_revision: None,
                peer_roster_hash: None,
                nonce: "proof_nonce_a".into(),
                jti: "proof_jti_a".into(),
                issued_at: 100,
                expires_at: 120,
            },
            endpoint_key,
            signature: URL_SAFE_NO_PAD.encode([0_u8; 64]),
        }
    }

    fn mobile_hello() -> Value {
        serde_json::to_value(MobileHello {
            kind: "mobile_hello".into(),
            schema_version: 2,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            session_nonce: "session_a".into(),
            endpoint_proof: fake_hello_proof(AdmissionRole::Mobile, "device_a"),
        })
        .unwrap()
    }

    #[test]
    fn direction_parsers_reject_unknown_fields_and_opposite_frames() {
        let valid = mobile_hello();
        assert!(matches!(
            parse_mobile_frame(&valid.to_string()),
            Ok(MobileInbound::Hello(_))
        ));
        assert_eq!(
            parse_plugin_frame(&valid.to_string()).unwrap_err(),
            V2ProtocolError::WrongDirection
        );

        let mut extra = valid;
        extra["admin"] = json!(true);
        assert_eq!(
            parse_mobile_frame(&extra.to_string()).unwrap_err(),
            V2ProtocolError::Malformed
        );
    }

    #[test]
    fn idle_frames_are_strict_typed_and_direction_bound() {
        let plugin_idle = json!({
            "kind": "plugin_idle",
            "schemaVersion": 2,
            "appId": "app_a",
            "eventId": "idle_event_a"
        });
        assert!(matches!(
            parse_plugin_frame(&plugin_idle.to_string()),
            Ok(PluginInbound::Idle(_))
        ));
        assert_eq!(
            parse_mobile_frame(&plugin_idle.to_string()).unwrap_err(),
            V2ProtocolError::WrongDirection
        );

        let mut unknown_plugin_field = plugin_idle;
        unknown_plugin_field["callId"] = json!("invented_call");
        assert_eq!(
            parse_plugin_frame(&unknown_plugin_field.to_string()).unwrap_err(),
            V2ProtocolError::Malformed
        );

        let idle_sync = MobileIdleSyncFrame {
            kind: "idle_sync".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            sequence: 1,
            grants: vec![Grant::StateRead, Grant::Monitor],
        };
        idle_sync.validate().unwrap();
        let mut unknown_mobile_field = serde_json::to_value(&idle_sync).unwrap();
        unknown_mobile_field["snapshot"] = json!({});
        assert!(serde_json::from_value::<MobileIdleSyncFrame>(unknown_mobile_field).is_err());

        let mut invalid = idle_sync.clone();
        invalid.sequence = 0;
        assert_eq!(
            invalid.validate().unwrap_err(),
            V2ProtocolError::Invalid("sequence")
        );
        invalid.sequence = 1;
        invalid.grants = vec![Grant::Monitor];
        assert_eq!(
            invalid.validate().unwrap_err(),
            V2ProtocolError::Invalid("grants")
        );
        invalid.grants = vec![Grant::StateRead, Grant::StateRead];
        assert_eq!(
            invalid.validate().unwrap_err(),
            V2ProtocolError::Invalid("grants")
        );
    }

    #[test]
    fn rfc7638_ed25519_thumbprint_matches_published_interop_vector() {
        // RFC 8032 test-vector public key, encoded as an OKP JWK `x`. PHP,
        // Desktop and mobile implementations use this same expected value.
        let public_key = URL_SAFE_NO_PAD
            .decode("11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo")
            .expect("published vector is base64url");
        let public_key: [u8; 32] = public_key.try_into().unwrap();
        let key = EndpointPublicKey::from_ed25519_bytes(&public_key);
        assert_eq!(
            key.public_key,
            "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
        );
        assert_eq!(
            key.thumbprint,
            "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k"
        );
        let canonical = json!({
            "x": key.public_key,
            "kty": "OKP",
            "crv": "Ed25519"
        });
        assert_eq!(
            String::from_utf8(canonical_json_value(&canonical).unwrap()).unwrap(),
            "{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":\"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo\"}"
        );
    }

    #[test]
    fn admission_and_hello_peer_policies_reject_the_holder_as_its_own_peer() {
        let holder = test_endpoint_key(7).thumbprint;
        let peer = test_endpoint_key(8).thumbprint;
        let mut mobile_admission = AdmissionClaims {
            aud: ADMISSION_AUDIENCE.into(),
            app_id: "app_a".into(),
            subject_id: "device_a".into(),
            role: AdmissionRole::Mobile,
            holder_key_thumbprint: holder.clone(),
            expected_peer_key_thumbprint: Some(holder.clone()),
            approved_peer_key_thumbprints: vec![],
            peer_roster_revision: None,
            peer_roster_hash: None,
            scopes: vec![Grant::StateRead],
            exp: 120,
            jti: "admission_mobile_a".into(),
        };
        assert_eq!(
            mobile_admission.validate(100).unwrap_err(),
            V2ProtocolError::Invalid("expectedPeerKeyThumbprint")
        );
        mobile_admission.expected_peer_key_thumbprint = Some(peer.clone());
        mobile_admission.validate(100).unwrap();

        let mut plugin_roster = vec![holder.clone(), peer.clone()];
        plugin_roster.sort();
        let mut plugin_admission = AdmissionClaims {
            aud: ADMISSION_AUDIENCE.into(),
            app_id: "app_a".into(),
            subject_id: "plugin_a".into(),
            role: AdmissionRole::Plugin,
            holder_key_thumbprint: holder.clone(),
            expected_peer_key_thumbprint: None,
            approved_peer_key_thumbprints: plugin_roster.clone(),
            peer_roster_revision: Some(1),
            peer_roster_hash: Some(peer_roster_hash(1, &plugin_roster)),
            scopes: vec![Grant::StateRead],
            exp: 120,
            jti: "admission_plugin_a".into(),
        };
        assert_eq!(
            plugin_admission.validate(100).unwrap_err(),
            V2ProtocolError::Invalid("approvedPeerKeyThumbprints")
        );
        plugin_admission.approved_peer_key_thumbprints = vec![peer.clone()];
        plugin_admission.peer_roster_hash = Some(peer_roster_hash(1, &[peer.clone()]));
        plugin_admission.validate(100).unwrap();

        let mut mobile_hello = fake_hello_proof(AdmissionRole::Mobile, "device_a").claims;
        mobile_hello.holder_key_thumbprint = holder.clone();
        mobile_hello.expected_peer_key_thumbprint = Some(holder.clone());
        assert_eq!(
            mobile_hello.validate(101).unwrap_err(),
            V2ProtocolError::Invalid("expectedPeerKeyThumbprint")
        );

        let mut plugin_hello = fake_hello_proof(AdmissionRole::Plugin, "plugin_a").claims;
        plugin_hello.holder_key_thumbprint = holder.clone();
        plugin_hello.expected_peer_key_thumbprint = None;
        plugin_hello.approved_peer_key_thumbprints = plugin_roster;
        plugin_hello.peer_roster_revision = Some(1);
        plugin_hello.peer_roster_hash = Some(peer_roster_hash(
            1,
            &plugin_hello.approved_peer_key_thumbprints,
        ));
        assert_eq!(
            plugin_hello.validate(101).unwrap_err(),
            V2ProtocolError::Invalid("approvedPeerKeyThumbprints")
        );
    }

    fn signed_binding(seed: u8, now: u64) -> (String, SignedEndpointBinding) {
        let signer = test_signing_key(seed);
        let endpoint_key =
            EndpointPublicKey::from_ed25519_bytes(&signer.verifying_key().to_bytes());
        let digest = std::iter::repeat("AA")
            .take(32)
            .collect::<Vec<_>>()
            .join(":");
        let sdp = format!("v=0\r\na=fingerprint:sha-256 {digest}\r\n");
        let claims = EndpointBindingClaims {
            app_id: "app_a".into(),
            plugin_id: "plugin_a".into(),
            device_id: "device_a".into(),
            rtc_session_id: "rtc_a".into(),
            endpoint_session_nonce: "mobile_session_a".into(),
            lease_jti: "lease_a".into(),
            endpoint_role: AdmissionRole::Mobile,
            holder_key_thumbprint: endpoint_key.thumbprint.clone(),
            peer_key_thumbprint: "plugin_thumbprint_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 3,
            fence: 9,
            sdp_revision: 1,
            transport_generation: 1,
            dtls_fingerprint: sdp_dtls_fingerprint(&sdp).unwrap(),
            sdp_sha256: sdp_sha256(&sdp),
            nonce: "binding_nonce_a".into(),
            jti: "binding_jti_a".into(),
            issued_at: now,
            expires_at: now + 20,
        };
        let signature =
            URL_SAFE_NO_PAD.encode(signer.sign(&claims.signing_bytes().unwrap()).to_bytes());
        (
            sdp,
            SignedEndpointBinding {
                endpoint_key,
                claims,
                signature,
            },
        )
    }

    #[test]
    fn endpoint_binding_rejects_substitution_stale_peer_call_and_revision() {
        let (sdp, binding) = signed_binding(9, 100);
        binding.verify_for_sdp(&sdp, 101).unwrap();

        let mut stale = binding.clone();
        assert!(stale.verify_for_sdp(&sdp, 121).is_err());
        stale.claims.expires_at = 200;
        assert!(stale.verify_for_sdp(&sdp, 101).is_err());

        let mut wrong_peer = binding.clone();
        wrong_peer.claims.peer_key_thumbprint = "other_plugin_thumbprint".into();
        assert!(wrong_peer.verify_for_sdp(&sdp, 101).is_err());

        let mut wrong_call = binding.clone();
        wrong_call.claims.call_id = "call_b".into();
        assert!(wrong_call.verify_for_sdp(&sdp, 101).is_err());

        let mut wrong_revision = binding.clone();
        wrong_revision.claims.sdp_revision = 2;
        assert!(wrong_revision.verify_for_sdp(&sdp, 101).is_err());

        let mut substituted_key = binding;
        substituted_key.endpoint_key = test_endpoint_key(10);
        assert!(substituted_key.verify_for_sdp(&sdp, 101).is_err());
    }

    #[test]
    fn signed_candidate_is_exact_and_rejects_candidate_transplant() {
        let signer = test_signing_key(11);
        let endpoint_key =
            EndpointPublicKey::from_ed25519_bytes(&signer.verifying_key().to_bytes());
        let claims = TrickleCandidateClaims {
            app_id: "app_a".into(),
            plugin_id: "plugin_a".into(),
            device_id: "device_a".into(),
            rtc_session_id: "rtc_a".into(),
            endpoint_session_nonce: "mobile_session_a".into(),
            lease_jti: "lease_a".into(),
            endpoint_role: AdmissionRole::Mobile,
            holder_key_thumbprint: endpoint_key.thumbprint.clone(),
            peer_key_thumbprint: "plugin_thumbprint_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 3,
            fence: 9,
            sdp_revision: 1,
            transport_generation: 1,
            candidate: Some("candidate:1 1 UDP 1 127.0.0.1 9 typ host".into()),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(0),
            end_of_candidates: false,
            nonce: "candidate_nonce_a".into(),
            jti: "candidate_jti_a".into(),
            issued_at: 100,
            expires_at: 120,
        };
        let envelope = SignedTrickleCandidateEnvelope {
            signature: URL_SAFE_NO_PAD
                .encode(signer.sign(&claims.signing_bytes().unwrap()).to_bytes()),
            endpoint_key,
            claims,
        };
        envelope
            .verify_for_candidate(
                Some("candidate:1 1 UDP 1 127.0.0.1 9 typ host"),
                Some("0"),
                Some(0),
                false,
                101,
            )
            .unwrap();
        assert!(envelope
            .verify_for_candidate(
                Some("candidate:2 1 UDP 1 127.0.0.1 10 typ host"),
                Some("0"),
                Some(0),
                false,
                101,
            )
            .is_err());
    }

    #[test]
    fn signal_shape_and_sizes_are_fail_closed() {
        let endpoint_key = test_endpoint_key(7);
        let sdp = "v=0".to_string();
        let binding = SignedEndpointBinding {
            claims: EndpointBindingClaims {
                app_id: "app_a".into(),
                plugin_id: "plugin_a".into(),
                device_id: "device_a".into(),
                rtc_session_id: "rtc_a".into(),
                endpoint_session_nonce: "session_a".into(),
                lease_jti: "lease_a".into(),
                endpoint_role: AdmissionRole::Mobile,
                holder_key_thumbprint: endpoint_key.thumbprint.clone(),
                peer_key_thumbprint: "plugin_thumbprint_a".into(),
                call_id: "call_a".into(),
                call_epoch: 1,
                owner_epoch: 2,
                fence: 3,
                sdp_revision: 1,
                transport_generation: 1,
                dtls_fingerprint: "sha-256 AA".into(),
                sdp_sha256: sdp_sha256(&sdp),
                nonce: "rtc_nonce_a".into(),
                jti: "rtc_jti_a".into(),
                issued_at: 100,
                expires_at: 120,
            },
            endpoint_key,
            signature: URL_SAFE_NO_PAD.encode([0_u8; 64]),
        };
        let mobile = serde_json::to_value(MobileRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: 2,
            app_id: "app_a".into(),
            signal_id: "signal_a".into(),
            plugin_id: "plugin_a".into(),
            device_id: "device_a".into(),
            lease_token: "token".into(),
            lease_jti: "lease_a".into(),
            rtc_session_id: "rtc_a".into(),
            sdp_revision: 1,
            transport_generation: 1,
            call_id: "call_a".into(),
            call_epoch: 1,
            owner_epoch: 2,
            fence: 3,
            signal: RtcSignal::Offer { sdp, binding },
        })
        .unwrap();
        assert!(matches!(
            parse_mobile_frame(&mobile.to_string()),
            Ok(MobileInbound::RtcSignal(_))
        ));

        let mut nested_extra = mobile.clone();
        nested_extra["signal"]["admin"] = json!(true);
        assert_eq!(
            parse_mobile_frame(&nested_extra.to_string()).unwrap_err(),
            V2ProtocolError::Malformed
        );

        let mut too_large = mobile;
        too_large["signal"]["sdp"] = json!("x".repeat(MAX_SDP_BYTES + 1));
        assert!(parse_mobile_frame(&too_large.to_string()).is_err());
    }

    #[test]
    fn mobile_offer_answer_requires_an_explicit_mode() {
        let answer = json!({
            "kind":"mobile_offer_answer", "schemaVersion":2, "appId":"app_a",
            "requestId":"request_a", "idempotencyKey":"answer-key-a",
            "offerId":"offer_a", "offerJti":"offer_jti_a", "offerToken":"token_a",
            "targetDeviceId":"device_a", "targetHolderKeyThumbprint":"mobile_thumbprint_a",
            "offeredMode":"takeover", "callId":"call_a", "callEpoch":7, "ownerEpoch":3
        });
        assert!(matches!(
            parse_mobile_frame(&answer.to_string()),
            Ok(MobileInbound::OfferAnswer(_))
        ));

        let mut missing_mode = answer;
        missing_mode.as_object_mut().unwrap().remove("offeredMode");
        assert_eq!(
            parse_mobile_frame(&missing_mode.to_string()).unwrap_err(),
            V2ProtocolError::Malformed
        );
    }

    #[test]
    fn snapshot_capability_secondary_evidence_and_audio_levels_fail_closed() {
        let snapshot = AuthoritativeCallSnapshot {
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 3,
            switchboard_revision: 11,
            remote_revision: 13,
            telephony_state: TelephonyState::Active,
            service_mode: ServiceMode::AokieActive,
            media_state: MediaState::Ready,
            remote_capabilities: RemoteCapabilities {
                software_hold: true,
                carrier_hold_evidence: CarrierHoldEvidence::Unknown,
                secondary_call_observation: SecondaryCallObservation::Unknown,
                voice_consult: true,
                takeover: true,
            },
            secondary_call_policy: SecondaryCallPolicy::Normal,
            secondary_call: None,
            remote_consent: RemoteConsentPolicy {
                policy_id: "remote_policy".into(),
                policy_version: 3,
                enabled: true,
                acknowledged: true,
                acknowledged_at: Some("2026-07-16T00:00:00Z".into()),
                expires_at: Some("2999-01-01T00:00:00Z".into()),
                captions_enabled: true,
                assistance_enabled: true,
                monitor_enabled: true,
                consult_enabled: true,
                takeover_enabled: true,
            },
            caller: None,
            captions: vec![],
            audio_levels: None,
            occurred_at: "2026-07-16T00:00:00Z".into(),
        };
        snapshot.validate().unwrap();

        let mut unsupported_consult = snapshot.clone();
        unsupported_consult.remote_capabilities.software_hold = false;
        assert!(matches!(
            unsupported_consult.validate(),
            Err(V2ProtocolError::Unsafe(_))
        ));

        let mut unobserved_secondary = snapshot.clone();
        unobserved_secondary.secondary_call = Some(ScreenedSecondaryCall {
            stable: false,
            callback_eligible: false,
            status: SecondaryCallStatus::Queued,
            waiting_call_id: None,
        });
        assert!(matches!(
            unobserved_secondary.validate(),
            Err(V2ProtocolError::Unsafe(_))
        ));

        let mut unstable_waiting_call = snapshot.clone();
        unstable_waiting_call
            .remote_capabilities
            .secondary_call_observation = SecondaryCallObservation::Observed;
        unstable_waiting_call.secondary_call_policy = SecondaryCallPolicy::MissAndCallback;
        unstable_waiting_call.secondary_call = Some(ScreenedSecondaryCall {
            stable: false,
            callback_eligible: true,
            status: SecondaryCallStatus::Queued,
            waiting_call_id: Some("waiting_call_a".into()),
        });
        assert!(matches!(
            unstable_waiting_call.validate(),
            Err(V2ProtocolError::Unsafe(_))
        ));

        let mut impossible_level = snapshot;
        impossible_level.audio_levels = Some(vec![NormalizedAudioLevel {
            source: AudioLevelSource::Caller,
            participant_id: None,
            level_permille: 1_001,
        }]);
        assert_eq!(
            impossible_level.validate().unwrap_err(),
            V2ProtocolError::Invalid("audioLevels.levelPermille")
        );
    }

    #[test]
    fn caller_ending_is_directional_one_use_shaped_and_failure_typed() {
        let confirm = json!({
            "kind":"end_caller_confirm", "schemaVersion":2, "appId":"app_a",
            "requestId":"request_a", "idempotencyKey":"key-a",
            "confirmationId":"confirmation_a", "nonce":"nonce_a",
            "leaseToken":"v2.redacted.signature", "callId":"call_a",
            "callEpoch":7, "ownerEpoch":3, "switchboardRevision":11,
            "remoteRevision":13, "fence":9
        });
        assert!(matches!(
            parse_mobile_frame(&confirm.to_string()),
            Ok(MobileInbound::EndCallerConfirm(_))
        ));
        assert_eq!(
            parse_plugin_frame(&confirm.to_string()).unwrap_err(),
            V2ProtocolError::WrongDirection
        );
        let mut missing_nonce = confirm;
        missing_nonce.as_object_mut().unwrap().remove("nonce");
        assert!(parse_mobile_frame(&missing_nonce.to_string()).is_err());

        let result = PluginEndCallerResultFrame {
            kind: "end_caller_result".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            operation_id: "operation_a".into(),
            confirmation_id: "confirmation_a".into(),
            device_id: "device_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 3,
            switchboard_revision: 11,
            remote_revision: 13,
            lease_id: "lease_a".into(),
            fence: 9,
            outcome: EndCallerOutcome::Failed,
            code: Some("radio_hangup_failed".into()),
            message: Some("physical hangup failed".into()),
        };
        result.validate().unwrap();
        let mut invalid = result;
        invalid.message = None;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn lease_track_matrix_gives_monitor_both_call_directions_without_a_talk_fence() {
        let now = 100;
        let monitor = LeaseClaims {
            aud: LEASE_AUDIENCE.into(),
            app_id: "app_a".into(),
            plugin_id: "plugin_a".into(),
            device_id: "device_a".into(),
            plugin_key_thumbprint: "plugin_thumbprint_a".into(),
            mobile_key_thumbprint: "mobile_thumbprint_a".into(),
            call_id: "call_a".into(),
            call_epoch: 1,
            owner_epoch: 1,
            mode: LeaseMode::Monitor,
            phase: LeasePhase::Active,
            tracks: tracks_for(LeaseMode::Monitor, LeasePhase::Active),
            expires_at: 110,
            lease_id: "stable_lease_a".into(),
            jti: "lease_a".into(),
            fence: 0,
            session_nonce: "session_a".into(),
            rtc_session_id: "rtc_a".into(),
        };
        monitor.validate(now).unwrap();

        assert_eq!(
            monitor.tracks,
            vec![MediaTrack::PstnIn, MediaTrack::PstnOut]
        );
        assert_eq!(monitor.fence, 0);
        let mut unsafe_monitor = monitor;
        unsafe_monitor.tracks.push(MediaTrack::ConsultRx);
        assert!(matches!(
            unsafe_monitor.validate(now),
            Err(V2ProtocolError::Unsafe(_))
        ));
    }

    #[test]
    fn talk_leases_require_positive_safe_fence() {
        let claims = LeaseClaims {
            aud: LEASE_AUDIENCE.into(),
            app_id: "app_a".into(),
            plugin_id: "plugin_a".into(),
            device_id: "device_a".into(),
            plugin_key_thumbprint: "plugin_thumbprint_a".into(),
            mobile_key_thumbprint: "mobile_thumbprint_a".into(),
            call_id: "call_a".into(),
            call_epoch: 1,
            owner_epoch: 1,
            mode: LeaseMode::Takeover,
            phase: LeasePhase::Prepared,
            tracks: tracks_for(LeaseMode::Takeover, LeasePhase::Prepared),
            expires_at: 110,
            lease_id: "stable_lease_a".into(),
            jti: "lease_a".into(),
            fence: 0,
            session_nonce: "session_a".into(),
            rtc_session_id: "rtc_a".into(),
        };
        assert!(matches!(
            claims.validate(100),
            Err(V2ProtocolError::Unsafe(_))
        ));
    }

    #[test]
    fn consultation_is_receive_only_until_the_rotated_active_lease() {
        assert_eq!(
            tracks_for(LeaseMode::Consult, LeasePhase::Prepared),
            vec![MediaTrack::ConsultRx]
        );
        assert_eq!(
            tracks_for(LeaseMode::Consult, LeasePhase::Active),
            vec![MediaTrack::ConsultRx, MediaTrack::ConsultTx]
        );

        let mut consent = RemoteConsentPolicy {
            policy_id: "remote_policy".into(),
            policy_version: 3,
            enabled: true,
            acknowledged: true,
            acknowledged_at: Some("2026-07-16T00:00:00Z".into()),
            expires_at: Some("2999-01-01T00:00:00Z".into()),
            captions_enabled: true,
            assistance_enabled: true,
            monitor_enabled: true,
            consult_enabled: false,
            takeover_enabled: true,
        };
        consent.validate().unwrap();
        assert!(!consent.allows(LeaseMode::Consult));
        consent.consult_enabled = true;
        assert!(consent.allows(LeaseMode::Consult));

        consent.acknowledged = false;
        consent.acknowledged_at = None;
        assert!(matches!(
            consent.validate(),
            Err(V2ProtocolError::Unsafe(_))
        ));
    }
}
