//! Admission responses: session credentials, ICE validation, relay endpoints.

#[allow(unused_imports)]
use super::*;

#[derive(Clone)]
pub(super) struct SessionCredentials {
    pub(super) endpoint: Url,
    pub(super) token: String,
    pub(super) app_id: String,
    pub(super) plugin_id: String,
    pub(super) lifetime: Duration,
    pub(super) ice_servers: Vec<IceServerConfig>,
    pub(super) relay_only: bool,
    pub(super) turn_credential_expires_at: Option<u64>,
    pub(super) endpoint_authority: Arc<EndpointAuthority>,
    /// Present only when the admission advertised the hosted relay AND every
    /// advertised URL passed [`normalize_relay_url`]. `None` selects the
    /// WebSocket gateway, which stays the default transport.
    pub(super) relay: Option<RelayEndpoints>,
}

impl fmt::Debug for SessionCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionCredentials")
            .field("endpoint", &redacted_url(self.endpoint.as_str()))
            .field("token", &"[REDACTED]")
            .field("app_id", &self.app_id)
            .field("plugin_id", &self.plugin_id)
            .field("lifetime", &self.lifetime)
            .field("ice_server_count", &self.ice_servers.len())
            .field("relay_only", &self.relay_only)
            .field(
                "has_expiring_turn_credentials",
                &self.turn_credential_expires_at.is_some(),
            )
            .field(
                "endpoint_key_thumbprint",
                &self.endpoint_authority.endpoint_key.thumbprint,
            )
            .field(
                "peer_roster_revision",
                &self.endpoint_authority.roster_revision,
            )
            .field("transport", &self.transport_label())
            .finish()
    }
}

impl SessionCredentials {
    pub(super) fn initial(
        bootstrap: &CompanionBootstrap,
        endpoint_authority: Arc<EndpointAuthority>,
    ) -> Result<Option<Self>, String> {
        let (Some(gateway_url), Some(access_token), Some(app_id)) = (
            bootstrap.gateway_url.as_deref(),
            bootstrap.access_token.as_deref(),
            bootstrap.app_id.as_deref(),
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            endpoint: normalize_gateway_url(gateway_url)?,
            token: access_token.to_string(),
            app_id: app_id.to_string(),
            plugin_id: bootstrap.plugin_id.clone(),
            lifetime: DEFAULT_BOOTSTRAP_LIFETIME,
            ice_servers: bootstrap.ice_servers.clone(),
            relay_only: bootstrap.relay_only,
            turn_credential_expires_at: None,
            endpoint_authority,
            // plugin.init's compact bootstrap carries no transport
            // advertisement; the first brokered admission refresh decides.
            relay: None,
        }))
    }

    pub(super) fn transport_label(&self) -> &'static str {
        if self.relay.is_some() {
            "relay"
        } else {
            "websocket"
        }
    }
}

/// FormLogic attaches credential expiry to each TURN entry. The native media
/// crate deliberately receives only the browser/WebRTC fields after this
/// broker response has been validated and its lifetime has been bounded.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AdmissionIceServer {
    pub(super) urls: Vec<String>,
    pub(super) username: String,
    pub(super) credential: String,
    #[serde(default, deserialize_with = "deserialize_optional_unix_timestamp")]
    pub(super) expires_at: Option<u64>,
}

pub(super) fn deserialize_optional_unix_timestamp<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::Number(number) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected a non-negative Unix timestamp")),
        _ => Err(serde::de::Error::custom("expected a Unix timestamp")),
    }
}

impl AdmissionIceServer {
    pub(super) fn runtime_config(&self) -> IceServerConfig {
        IceServerConfig {
            urls: self.urls.clone(),
            username: self.username.clone(),
            credential: self.credential.clone(),
        }
    }

    pub(super) fn has_turn_url(&self) -> bool {
        self.urls.iter().any(|url| {
            let lower = url.to_ascii_lowercase();
            lower.starts_with("turn:") || lower.starts_with("turns:")
        })
    }
}

/// A transparent wrapper makes the JSON member itself mandatory while still
/// accepting the backend's explicit `null` when no TURN server is configured.
pub(super) struct NullableUnixTimestamp(pub(super) Option<u64>);

impl<'de> Deserialize<'de> for NullableUnixTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::Null => Ok(Self(None)),
            Value::Number(number) => number
                .as_u64()
                .map(|value| Self(Some(value)))
                .ok_or_else(|| serde::de::Error::custom("expected a non-negative Unix timestamp")),
            _ => Err(serde::de::Error::custom(
                "expected a Unix timestamp or null",
            )),
        }
    }
}

impl NullableUnixTimestamp {
    pub(super) fn value(self) -> Option<u64> {
        self.0
    }
}

/// FormLogic-hosted relay transport advertised alongside the WebSocket
/// gateway. Absent (the default) keeps the untouched WebSocket path, so
/// withdrawing the member server-side reverts the transport with no rebuild.
///
/// ⚠️ Deliberately NOT `deny_unknown_fields`, unlike every security-bearing
/// document around it. This is an additive transport hint: a server that later
/// advertises another member (the long-poll fallback the relay controller
/// already serves is the obvious next one) must leave this build using the
/// three URLs it does understand, not lose the whole admission over a member
/// it was never taught.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RelayEndpoints {
    pub(crate) challenge_url: String,
    pub(crate) frames_url: String,
    pub(crate) stream_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AdmissionResponse {
    pub(super) access_token: String,
    pub(super) token_type: String,
    pub(super) expires_in: u64,
    pub(super) expires_at: u64,
    pub(super) gateway_url: String,
    pub(super) app_id: String,
    pub(super) subject_id: String,
    pub(super) role: String,
    pub(super) scopes: Vec<String>,
    pub(super) device: Value,
    pub(super) ice_servers: Vec<AdmissionIceServer>,
    pub(super) relay_only: bool,
    pub(super) turn_credential_expires_at: NullableUnixTimestamp,
    pub(super) endpoint_public_key: EndpointPublicKey,
    pub(super) holder_key_thumbprint: String,
    pub(super) approved_peer_key_thumbprints: Vec<String>,
    pub(super) peer_roster_revision: u64,
    pub(super) peer_roster_hash: String,
    /// Tolerated ahead of the Desktop projection that forwards it: this
    /// decoder is `deny_unknown_fields`, so the member has to be accepted
    /// before it can ever arrive.
    ///
    /// Held as a raw value rather than a typed member ON PURPOSE. Decoding it
    /// inline would make a malformed or reshaped advertisement fail the whole
    /// admission — a `rebootstrap` that takes the entire Companion surface
    /// down — when the transport is additive and the correct answer is to keep
    /// using the WebSocket gateway. [`usable_relay_endpoints`] owns that
    /// decision, so every rejection lands on the same degrade path.
    #[serde(default)]
    pub(super) relay: Option<Value>,
}

impl AdmissionResponse {
    pub(super) fn into_credentials(
        self,
        expected_app_id: Option<&str>,
        expected_plugin_id: &str,
        endpoint_authority: Arc<EndpointAuthority>,
    ) -> Result<SessionCredentials, WorkerError> {
        let _ = (&self.scopes, &self.device);
        if self.token_type != "Bearer"
            || self.role != "plugin"
            || expected_app_id.is_some_and(|expected| self.app_id != expected)
            || self.subject_id != expected_plugin_id
            || self.endpoint_public_key != endpoint_authority.endpoint_key
            || self.holder_key_thumbprint != endpoint_authority.endpoint_key.thumbprint
            || self.approved_peer_key_thumbprints != endpoint_authority.approved_thumbprints()
            || self.peer_roster_revision != endpoint_authority.roster_revision
            || self.peer_roster_hash != endpoint_authority.roster_hash
        {
            return Err(WorkerError::rebootstrap(
                "Desktop returned a Companion admission for a different identity",
            ));
        }
        validate_identity(&self.app_id, "appId").map_err(WorkerError::rebootstrap)?;
        validate_identity(&self.subject_id, "subjectId").map_err(WorkerError::rebootstrap)?;
        validate_bearer(&self.access_token).map_err(WorkerError::rebootstrap)?;
        let now = unix_now()?;
        let wall_remaining = self.expires_at.saturating_sub(now);
        if self.expires_in <= ADMISSION_SAFETY_MARGIN_SECONDS
            || self.expires_in > 300
            || wall_remaining <= ADMISSION_SAFETY_MARGIN_SECONDS
            || wall_remaining > 300
        {
            return Err(WorkerError::expired(
                "Desktop returned an expired or unsafe Companion admission lifetime",
            ));
        }
        let turn_credential_expires_at = self.turn_credential_expires_at.value();
        let ice_servers = validate_admission_ice_configuration(
            &self.ice_servers,
            self.relay_only,
            turn_credential_expires_at,
            now,
        )
        .map_err(|_| {
            WorkerError::rebootstrap("Desktop returned invalid or unsafe ICE server settings")
        })?;
        let mut safe_remaining = self.expires_in.min(wall_remaining);
        if let Some(expiry) = turn_credential_expires_at {
            safe_remaining = safe_remaining.min(expiry.saturating_sub(now));
        }
        if safe_remaining <= ADMISSION_SAFETY_MARGIN_SECONDS {
            return Err(WorkerError::expired(
                "Desktop returned ICE credentials with no safe connection lifetime",
            ));
        }
        Ok(SessionCredentials {
            endpoint: normalize_gateway_url(&self.gateway_url).map_err(WorkerError::rebootstrap)?,
            token: self.access_token,
            app_id: self.app_id,
            plugin_id: self.subject_id,
            lifetime: Duration::from_secs(
                safe_remaining.saturating_sub(ADMISSION_SAFETY_MARGIN_SECONDS),
            ),
            ice_servers,
            relay_only: self.relay_only,
            turn_credential_expires_at,
            endpoint_authority,
            relay: self.relay.and_then(usable_relay_endpoints),
        })
    }
}

/// Accept an advertised relay only when it decodes to the shape this build
/// understands, every URL is safe, AND all three share one origin. A rejected
/// advertisement degrades to the WebSocket gateway rather than failing the
/// admission: the transport is additive, and refusing the whole admission over
/// it would take the Companion surface down harder than simply not adopting
/// the new path.
pub(super) fn usable_relay_endpoints(advertisement: Value) -> Option<RelayEndpoints> {
    let relay: RelayEndpoints = match serde_json::from_value(advertisement) {
        Ok(relay) => relay,
        Err(_) => {
            eprintln!(
                "[aokie-plugin][companion] stage=relay_advertisement_rejected transport=websocket detail=The relay advertisement is not the shape this build understands"
            );
            return None;
        }
    };
    let checked = [
        normalize_relay_url(&relay.challenge_url, "challengeUrl"),
        normalize_relay_url(&relay.frames_url, "framesUrl"),
        normalize_relay_url(&relay.stream_url, "streamUrl"),
    ];
    let mut origins = Vec::with_capacity(checked.len());
    for outcome in &checked {
        match outcome {
            Ok(url) => origins.push(url.origin()),
            Err(error) => {
                eprintln!(
                    "[aokie-plugin][companion] stage=relay_advertisement_rejected transport=websocket detail={}",
                    sanitize_status_message(&error.message)
                );
                return None;
            }
        }
    }
    if origins.windows(2).any(|pair| pair[0] != pair[1]) {
        eprintln!(
            "[aokie-plugin][companion] stage=relay_advertisement_rejected transport=websocket detail=Companion relay URLs span more than one origin"
        );
        return None;
    }
    Some(relay)
}

pub(super) fn validate_admission_ice_configuration(
    servers: &[AdmissionIceServer],
    relay_only: bool,
    turn_credential_expires_at: Option<u64>,
    now: u64,
) -> Result<Vec<IceServerConfig>, String> {
    let runtime_servers = servers
        .iter()
        .map(AdmissionIceServer::runtime_config)
        .collect::<Vec<_>>();
    IceServerConfig::validate_all(&runtime_servers).map_err(|error| error.to_string())?;

    let mut earliest_turn_expiry = None;
    for server in servers {
        if server.has_turn_url() {
            if server.username.is_empty() || server.credential.is_empty() {
                return Err("TURN servers require short-lived credentials".into());
            }
            let expiry = server.expires_at.ok_or("TURN servers require expiresAt")?;
            if expiry <= now.saturating_add(MIN_TURN_CREDENTIAL_TTL_SECONDS)
                || expiry > now.saturating_add(MAX_TURN_CREDENTIAL_TTL_SECONDS)
            {
                return Err("TURN expiresAt must be 31 seconds to 24 hours in the future".into());
            }
            earliest_turn_expiry = Some(
                earliest_turn_expiry
                    .map(|current: u64| current.min(expiry))
                    .unwrap_or(expiry),
            );
        } else if !server.username.is_empty()
            || !server.credential.is_empty()
            || server.expires_at.is_some()
        {
            return Err("STUN-only entries cannot contain credentials or expiresAt".into());
        }
    }

    if relay_only && earliest_turn_expiry.is_none() {
        return Err("relayOnly requires at least one TURN server".into());
    }
    if earliest_turn_expiry != turn_credential_expires_at {
        return Err("turnCredentialExpiresAt does not match the earliest TURN expiry".into());
    }
    Ok(runtime_servers)
}
