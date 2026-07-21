//! Companion bootstrap material: identity, roster and the endpoint authority.

#[allow(unused_imports)]
use super::*;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionBootstrap {
    pub schema_version: u16,
    #[serde(default)]
    pub gateway_url: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    pub plugin_id: String,
    #[serde(default)]
    pub ice_servers: Vec<IceServerConfig>,
    #[serde(default)]
    pub relay_only: bool,
    pub endpoint_identity: EndpointIdentityBootstrap,
    pub approved_mobile_roster: ApprovedMobileRosterBootstrap,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointIdentityBootstrap {
    pub algorithm: EndpointKeyAlgorithm,
    pub public_key: String,
    pub thumbprint: String,
    pub private_key_seed: String,
}

impl fmt::Debug for EndpointIdentityBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EndpointIdentityBootstrap")
            .field("algorithm", &self.algorithm)
            .field("public_key", &self.public_key)
            .field("thumbprint", &self.thumbprint)
            .field("private_key_seed", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovedMobileRosterBootstrap {
    pub revision: u64,
    pub roster_hash: String,
    pub keys: Vec<EndpointPublicKey>,
}

#[derive(Clone)]
pub(super) struct EndpointAuthority {
    pub(super) signing_key: SigningKey,
    pub(super) endpoint_key: EndpointPublicKey,
    pub(super) roster_revision: u64,
    pub(super) roster_hash: String,
    pub(super) approved_mobile_keys: HashMap<String, EndpointPublicKey>,
}

impl EndpointAuthority {
    pub(super) fn from_bootstrap(
        identity: &EndpointIdentityBootstrap,
        roster: &ApprovedMobileRosterBootstrap,
    ) -> Result<Self, String> {
        let seed = URL_SAFE_NO_PAD
            .decode(&identity.private_key_seed)
            .map_err(|_| "privateBootstrap endpointIdentity.privateKeySeed is invalid")?;
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| "privateBootstrap endpointIdentity.privateKeySeed must be 32 bytes")?;
        let signing_key = SigningKey::from_bytes(&seed);
        let endpoint_key =
            EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
        if identity.algorithm != EndpointKeyAlgorithm::Ed25519
            || endpoint_key.public_key != identity.public_key
            || endpoint_key.thumbprint != identity.thumbprint
        {
            return Err(
                "privateBootstrap endpoint identity seed, public key and thumbprint disagree"
                    .into(),
            );
        }
        if roster.revision == 0 || roster.keys.is_empty() || roster.keys.len() > 64 {
            return Err(
                "privateBootstrap approvedMobileRoster must contain 1..64 keys at a positive revision"
                    .into(),
            );
        }
        let mut approved_mobile_keys = HashMap::new();
        for key in &roster.keys {
            key.validate()
                .map_err(|_| "privateBootstrap approvedMobileRoster contains an invalid key")?;
            if approved_mobile_keys
                .insert(key.thumbprint.clone(), key.clone())
                .is_some()
            {
                return Err(
                    "privateBootstrap approvedMobileRoster contains duplicate thumbprints".into(),
                );
            }
        }
        let mut thumbprints = approved_mobile_keys.keys().cloned().collect::<Vec<_>>();
        thumbprints.sort();
        if roster.roster_hash != peer_roster_hash(roster.revision, &thumbprints) {
            return Err("privateBootstrap approvedMobileRoster hash is invalid".into());
        }
        Ok(Self {
            signing_key,
            endpoint_key,
            roster_revision: roster.revision,
            roster_hash: roster.roster_hash.clone(),
            approved_mobile_keys,
        })
    }

    pub(super) fn approved_thumbprints(&self) -> Vec<String> {
        let mut thumbprints = self
            .approved_mobile_keys
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        thumbprints.sort();
        thumbprints
    }

    pub(super) fn sign(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key.sign(message).to_bytes())
    }
}

impl fmt::Debug for CompanionBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompanionBootstrap")
            .field("schema_version", &self.schema_version)
            .field(
                "gateway_url",
                &self.gateway_url.as_deref().map(redacted_url),
            )
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("app_id", &self.app_id)
            .field("plugin_id", &self.plugin_id)
            .field("ice_server_count", &self.ice_servers.len())
            .field("relay_only", &self.relay_only)
            .field("endpoint_identity", &self.endpoint_identity)
            .field("approved_mobile_roster", &self.approved_mobile_roster)
            .finish()
    }
}

impl CompanionBootstrap {
    pub fn parse(value: &Value) -> Result<Self, String> {
        let bootstrap: Self = serde_json::from_value(value.clone())
            .map_err(|_| "privateBootstrap has an invalid v2 shape".to_string())?;
        bootstrap.validate()?;
        Ok(bootstrap)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "privateBootstrap schemaVersion must be {SCHEMA_VERSION}"
            ));
        }
        validate_identity(&self.plugin_id, "pluginId")?;
        match (&self.gateway_url, &self.access_token, &self.app_id) {
            (Some(gateway_url), Some(access_token), Some(app_id)) => {
                validate_identity(app_id, "appId")?;
                validate_bearer(access_token)?;
                normalize_gateway_url(gateway_url)?;
                IceServerConfig::validate_all(&self.ice_servers)
                    .map_err(|error| error.to_string())?;
            }
            (None, None, None) if self.ice_servers.is_empty() && !self.relay_only => {}
            (None, None, None) => {
                return Err(
                    "privateBootstrap identity-only shape cannot include ICE settings".into(),
                )
            }
            _ => {
                return Err(
                    "privateBootstrap admission fields must be all present or all absent".into(),
                )
            }
        }
        EndpointAuthority::from_bootstrap(&self.endpoint_identity, &self.approved_mobile_roster)
            .map(|_| ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayConnectionPhase {
    Connecting,
    Connected,
    Reconnecting,
    AdmissionRefresh,
    Expired,
    RebootstrapRequired,
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatusSnapshot {
    pub configured: bool,
    pub connected: bool,
    pub phase: GatewayConnectionPhase,
    pub reconnect_attempt: u32,
    pub last_error: Option<String>,
    pub changed_at: String,
}

impl GatewayStatusSnapshot {
    pub(super) fn starting() -> Self {
        Self {
            configured: true,
            connected: false,
            phase: GatewayConnectionPhase::Connecting,
            reconnect_attempt: 0,
            last_error: None,
            changed_at: aokie_core::events::now_iso8601(),
        }
    }
}
