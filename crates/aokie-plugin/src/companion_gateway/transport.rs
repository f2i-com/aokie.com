//! Gateway transports: WebSocket carrier, greeting and admission rotation tasks.

#[allow(unused_imports)]
use super::*;

/// One session, two possible carriers. Every [`GatewaySession`] method already
/// speaks text in and text out, so the protocol itself is transport-blind: the
/// WebSocket gateway remains the default and the FormLogic-hosted relay is
/// selected only when an admission advertises it.
///
/// Exactly one of these exists per session, so the size gap between the
/// carriers buys nothing worth boxing the socket the live path runs on.
#[allow(clippy::large_enum_variant)]
pub(super) enum GatewayTransport {
    WebSocket(WebSocketTransport),
    #[cfg(feature = "voice")]
    Relay(crate::companion_relay::RelayChannel),
}

/// Carrier namespace whose authenticated connection state can survive an
/// admission rotation. WebSocket continuity keeps the gateway's established
/// behaviour. Relay continuity is narrower: its numeric cursor is meaningful
/// only inside one exact mailbox domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AdmissionCarrierDomain {
    WebSocket,
    #[cfg(feature = "voice")]
    Relay(crate::companion_relay::RelayCursorDomain),
}

impl AdmissionCarrierDomain {
    pub(super) fn for_credentials(credentials: &SessionCredentials) -> Result<Self, WorkerError> {
        #[cfg(feature = "voice")]
        if let Some(endpoints) = credentials.relay.as_ref() {
            let domain = crate::companion_relay::RelayCursorDomain::from_endpoints(
                endpoints,
                &credentials.app_id,
                &credentials.plugin_id,
            )
            .ok_or_else(|| WorkerError::rebootstrap("Companion relay cursor domain is invalid"))?;
            return Ok(Self::Relay(domain));
        }
        #[cfg(not(feature = "voice"))]
        let _ = credentials;
        Ok(Self::WebSocket)
    }

    pub(super) fn inherits_relay_cursor(&self) -> bool {
        #[cfg(feature = "voice")]
        {
            return matches!(self, Self::Relay(_));
        }
        #[cfg(not(feature = "voice"))]
        {
            false
        }
    }
}

/// The WebSocket carrier owns its own heartbeat bookkeeping so an admission
/// rotation replaces the ping schedule together with the socket it belongs to.
pub(super) struct WebSocketTransport {
    pub(super) socket: GatewaySocket,
    pub(super) next_ping: Instant,
    pub(super) awaiting_pong: Option<Instant>,
    /// Replacement sockets finish their endpoint challenge in the background
    /// but do not send this hello until the authority loop is ready to swap.
    /// Sending it earlier lets the gateway fence the predecessor before the
    /// loop has taken ownership of the replacement.
    pub(super) pending_hello: Option<String>,
}

/// A freshly challenged hello prepared away from the call-authority loop.
#[cfg(feature = "voice")]
#[derive(Debug)]
pub(super) struct FreshRelayGreeting {
    pub(super) channel_id: u64,
    pub(super) party: String,
    pub(super) plugin_session_nonce: String,
    pub(super) encoded_hello: String,
}

/// Bounded, per-party greeting refresh work.
///
/// Fetching the relay challenge may consume the full HTTP timeout. These tasks
/// own clone-only request/signing inputs and never borrow [`GatewayTransport`],
/// so `run_socket` continues receiving lease heartbeats and reconciling media
/// authority while the request is delayed. A repeated hello for the same party
/// coalesces onto its existing task.
#[cfg(feature = "voice")]
#[derive(Default)]
pub(super) struct RelayGreetingTasks {
    pub(super) by_party: HashMap<String, tokio::task::JoinHandle<Result<FreshRelayGreeting, WorkerError>>>,
}

#[cfg(feature = "voice")]
impl RelayGreetingTasks {
    pub(super) fn schedule(
        &mut self,
        party: String,
        request: crate::companion_relay::RelayGreetingRequest,
        credentials: SessionCredentials,
        plugin_session_nonce: String,
    ) {
        if self.by_party.contains_key(&party) {
            return;
        }
        let task_party = party.clone();
        let channel_id = request.channel_id();
        let handle = tokio::spawn(async move {
            let challenge = request.fetch_challenge().await?;
            let hello = endpoint_hello_for_session(
                &challenge,
                &credentials,
                unix_now()?,
                &plugin_session_nonce,
            )?;
            let encoded_hello = serde_json::to_string(&hello)
                .map_err(|_| WorkerError::reconnect("Companion frame could not be encoded"))?;
            Ok(FreshRelayGreeting {
                channel_id,
                party: task_party,
                plugin_session_nonce,
                encoded_hello,
            })
        });
        self.by_party.insert(party, handle);
    }

    /// Drain only tasks Tokio already marks complete. Awaiting one of these
    /// cannot inherit the challenge timeout; work still in flight remains in
    /// the map and the authority loop proceeds immediately.
    pub(super) async fn take_finished(&mut self) -> Vec<Result<FreshRelayGreeting, WorkerError>> {
        let finished = self
            .by_party
            .iter()
            .filter_map(|(party, task)| task.is_finished().then(|| party.clone()))
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(finished.len());
        for party in finished {
            let Some(task) = self.by_party.remove(&party) else {
                continue;
            };
            results.push(match task.await {
                Ok(result) => result,
                Err(_) => Err(WorkerError::reconnect(
                    "Companion relay greeting refresh task stopped",
                )),
            });
        }
        results
    }

    pub(super) fn abort_all(&mut self) {
        for (_, task) in self.by_party.drain() {
            task.abort();
        }
    }

    #[cfg(test)]
    pub(super) fn is_pending(&self, party: &str) -> bool {
        self.by_party.contains_key(party)
    }
}

#[cfg(feature = "voice")]
impl Drop for RelayGreetingTasks {
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// A fully authenticated replacement prepared without borrowing the carrier
/// or session whose authority it will supersede.
pub(super) struct OpenedAdmissionRotation {
    pub(super) generation: u64,
    pub(super) credentials: SessionCredentials,
    pub(super) transport: GatewayTransport,
    pub(super) plugin_session_nonce: String,
    pub(super) preserve_continuity: bool,
}

pub(super) struct PendingAdmissionBroker {
    pub(super) request_id: u64,
    pub(super) response: std::sync::mpsc::Receiver<crate::host_rpc::HostResult>,
    pub(super) started_at: Instant,
    pub(super) expected_app_id: String,
    pub(super) plugin_id: String,
    pub(super) endpoint_authority: Arc<EndpointAuthority>,
    pub(super) status: Arc<Mutex<GatewayStatusSnapshot>>,
    pub(super) attempt: u32,
    pub(super) predecessor_domain: AdmissionCarrierDomain,
}

pub(super) enum AdmissionRotationPhase {
    Broker(PendingAdmissionBroker),
    Opening(tokio::task::JoinHandle<Result<OpenedAdmissionRotation, WorkerError>>),
}

/// One overlapping admission replacement.
///
/// The Desktop RPC receiver is polled with `try_recv`, and endpoint opening is
/// owned by a Tokio task. Neither phase can wait on the sole session loop, so
/// the predecessor continues reading heartbeats and renewing active authority
/// until the replacement is complete. At most one generation is in flight.
pub(super) struct AdmissionRotationTask {
    pub(super) host_rpc: Arc<HostRpc>,
    pub(super) generation: u64,
    pub(super) phase: Option<AdmissionRotationPhase>,
}

impl AdmissionRotationTask {
    pub(super) fn begin(
        host_rpc: Arc<HostRpc>,
        generation: u64,
        credentials: &SessionCredentials,
        status: Arc<Mutex<GatewayStatusSnapshot>>,
        attempt: u32,
        predecessor_domain: AdmissionCarrierDomain,
    ) -> Result<Self, WorkerError> {
        let params = admission_request_params(
            Some(&credentials.app_id),
            &credentials.plugin_id,
            &credentials.endpoint_authority,
        )?;
        let (request_id, line, response) =
            host_rpc.begin("companion.admission", Value::Object(params));
        let mut sink = StdoutSink::new();
        if sink.send_line(&line).is_err() {
            host_rpc.forget(request_id);
            return Err(WorkerError::rebootstrap(
                "Desktop admission broker is unavailable",
            ));
        }
        Ok(Self {
            host_rpc,
            generation,
            phase: Some(AdmissionRotationPhase::Broker(PendingAdmissionBroker {
                request_id,
                response,
                started_at: Instant::now(),
                expected_app_id: credentials.app_id.clone(),
                plugin_id: credentials.plugin_id.clone(),
                endpoint_authority: credentials.endpoint_authority.clone(),
                status,
                attempt,
                predecessor_domain,
            })),
        })
    }

    /// Return only a completed result. Pending broker and transport work is
    /// observed, never awaited, by the authority loop.
    pub(super) async fn take_finished(&mut self) -> Option<Result<OpenedAdmissionRotation, WorkerError>> {
        let broker_result = match self.phase.as_mut() {
            Some(AdmissionRotationPhase::Broker(pending)) => {
                if pending.started_at.elapsed() >= ADMISSION_RPC_TIMEOUT {
                    Some(Err(WorkerError::rebootstrap(
                        "Desktop admission refresh timed out",
                    )))
                } else {
                    match pending.response.try_recv() {
                        Ok(Ok(value)) => Some(Ok(value)),
                        Ok(Err(_)) => Some(Err(WorkerError::rebootstrap(
                            "Desktop rejected Companion admission refresh",
                        ))),
                        Err(std::sync::mpsc::TryRecvError::Empty) => None,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(
                            WorkerError::rebootstrap("Desktop admission refresh stopped"),
                        )),
                    }
                }
            }
            _ => None,
        };

        if let Some(result) = broker_result {
            let Some(AdmissionRotationPhase::Broker(pending)) = self.phase.take() else {
                return Some(Err(WorkerError::reconnect(
                    "Companion admission rotation lost its broker state",
                )));
            };
            if result.is_err() {
                self.host_rpc.forget(pending.request_id);
            }
            let value = match result {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            let response: AdmissionResponse = match serde_json::from_value(value) {
                Ok(response) => response,
                Err(_) => {
                    return Some(Err(WorkerError::rebootstrap(
                        "Desktop returned an invalid Companion admission response",
                    )))
                }
            };
            let credentials = match response.into_credentials(
                Some(&pending.expected_app_id),
                &pending.plugin_id,
                pending.endpoint_authority,
            ) {
                Ok(credentials) => credentials,
                Err(error) => return Some(Err(error)),
            };
            let replacement_domain = match AdmissionCarrierDomain::for_credentials(&credentials) {
                Ok(domain) => domain,
                Err(error) => return Some(Err(error)),
            };
            let preserve_continuity = pending.predecessor_domain == replacement_domain;
            let inherit_relay_cursor =
                preserve_continuity && pending.predecessor_domain.inherits_relay_cursor();
            let generation = self.generation;
            let handle = tokio::spawn(async move {
                let opened = tokio::time::timeout(
                    ADMISSION_TRANSPORT_OPEN_TIMEOUT,
                    GatewayTransport::open_replacement(
                        &credentials,
                        &pending.status,
                        pending.attempt,
                        inherit_relay_cursor,
                    ),
                )
                .await
                .map_err(|_| {
                    WorkerError::reconnect(
                        "Companion replacement transport did not open before its deadline",
                    )
                })??;
                Ok(OpenedAdmissionRotation {
                    generation,
                    credentials,
                    transport: opened.0,
                    plugin_session_nonce: opened.1,
                    preserve_continuity,
                })
            });
            self.phase = Some(AdmissionRotationPhase::Opening(handle));
            return None;
        }

        let opening_finished = matches!(
            self.phase.as_ref(),
            Some(AdmissionRotationPhase::Opening(task)) if task.is_finished()
        );
        if !opening_finished {
            return None;
        }
        let Some(AdmissionRotationPhase::Opening(task)) = self.phase.take() else {
            return Some(Err(WorkerError::reconnect(
                "Companion admission rotation lost its transport state",
            )));
        };
        Some(match task.await {
            Ok(result) => result,
            Err(_) => Err(WorkerError::reconnect(
                "Companion admission rotation task stopped",
            )),
        })
    }

    #[cfg(all(test, feature = "voice"))]
    pub(super) fn from_opening_for_test(
        host_rpc: Arc<HostRpc>,
        generation: u64,
        task: tokio::task::JoinHandle<Result<OpenedAdmissionRotation, WorkerError>>,
    ) -> Self {
        Self {
            host_rpc,
            generation,
            phase: Some(AdmissionRotationPhase::Opening(task)),
        }
    }

    #[cfg(test)]
    pub(super) fn is_pending(&self) -> bool {
        self.phase.is_some()
    }
}

impl Drop for AdmissionRotationTask {
    fn drop(&mut self) {
        match self.phase.take() {
            Some(AdmissionRotationPhase::Broker(pending)) => {
                self.host_rpc.forget(pending.request_id);
            }
            Some(AdmissionRotationPhase::Opening(task)) => task.abort(),
            None => {}
        }
    }
}

impl GatewayTransport {
    pub(super) async fn open(
        credentials: &SessionCredentials,
        status: &Arc<Mutex<GatewayStatusSnapshot>>,
        attempt: u32,
    ) -> Result<(Self, String), WorkerError> {
        Self::open_inner(credentials, status, attempt, false, false).await
    }

    pub(super) async fn open_replacement(
        credentials: &SessionCredentials,
        status: &Arc<Mutex<GatewayStatusSnapshot>>,
        attempt: u32,
        inherit_relay_cursor: bool,
    ) -> Result<(Self, String), WorkerError> {
        Self::open_inner(credentials, status, attempt, inherit_relay_cursor, true).await
    }

    pub(super) async fn open_inner(
        credentials: &SessionCredentials,
        status: &Arc<Mutex<GatewayStatusSnapshot>>,
        attempt: u32,
        inherit_relay_cursor: bool,
        defer_websocket_hello: bool,
    ) -> Result<(Self, String), WorkerError> {
        #[cfg(feature = "voice")]
        if let Some(relay) = credentials.relay.as_ref() {
            let approved = credentials.endpoint_authority.approved_thumbprints();
            let (mut channel, challenge) = if inherit_relay_cursor {
                crate::companion_relay::RelayChannel::connect_replacement(
                    relay,
                    &credentials.token,
                    &credentials.app_id,
                    &credentials.plugin_id,
                    approved,
                )
                .await?
            } else {
                crate::companion_relay::RelayChannel::connect(
                    relay,
                    &credentials.token,
                    &credentials.app_id,
                    &credentials.plugin_id,
                    approved,
                )
                .await?
            };
            // The identical validation + signing the socket runs, so a relay
            // session proves the same endpoint identity from the same document.
            let (hello, session_nonce) = endpoint_hello(&challenge, credentials, unix_now()?)?;
            let encoded = serde_json::to_string(&hello)
                .map_err(|_| WorkerError::reconnect("Companion frame could not be encoded"))?;
            channel.arm(encoded);
            set_status(status, GatewayConnectionPhase::Connected, attempt, None);
            return Ok((Self::Relay(channel), session_nonce));
        }
        #[cfg(not(feature = "voice"))]
        if credentials.relay.is_some() {
            // The relay carrier rides reqwest, which only the voice build
            // pulls in. Say so once and use the proven WebSocket path rather
            // than pretending the advertisement was never made.
            eprintln!(
                "[aokie-plugin][companion] stage=relay_unavailable transport=websocket detail=The hosted relay transport requires the voice build"
            );
        }
        #[cfg(not(feature = "voice"))]
        let _ = inherit_relay_cursor;
        let (socket, session_nonce, pending_hello) =
            open_gateway_socket(credentials, status, attempt, defer_websocket_hello).await?;
        Ok((
            Self::WebSocket(WebSocketTransport {
                socket,
                next_ping: Instant::now() + PING_INTERVAL,
                awaiting_pong: None,
                pending_hello,
            }),
            session_nonce,
        ))
    }

    pub(super) async fn send_text(&mut self, encoded: &str) -> Result<TransportDelivery, WorkerError> {
        match self {
            Self::WebSocket(transport) => transport
                .socket
                .send(Message::Text(encoded.into()))
                .await
                .map(|_| TransportDelivery::Delivered)
                .map_err(safe_ws_error),
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.send_text(encoded).await,
        }
    }

    /// `Ok(None)` means "nothing for the session this tick" — the carrier's own
    /// keepalive traffic never reaches the protocol layer.
    pub(super) async fn recv_text(&mut self, tick: Duration) -> Result<Option<String>, WorkerError> {
        match self {
            Self::WebSocket(transport) => transport.recv_text(tick).await,
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.recv_text(tick).await,
        }
    }

    pub(super) async fn tick_heartbeat(&mut self) -> Result<(), WorkerError> {
        match self {
            Self::WebSocket(transport) => transport.tick_heartbeat().await,
            // The relay has no connection to keep warm: the server sends SSE
            // heartbeat comments and the reader tracks its own read idleness.
            #[cfg(feature = "voice")]
            Self::Relay(_) => Ok(()),
        }
    }

    /// Which carrier this session actually opened.
    ///
    /// Derived from the transport that was opened, never from
    /// `credentials.relay.is_some()`: the non-voice build advertises a relay in
    /// its admission request and still runs the socket, so the advertisement
    /// does not tell you which carrier is live.
    pub(super) fn is_relay(&self) -> bool {
        match self {
            Self::WebSocket(_) => false,
            #[cfg(feature = "voice")]
            Self::Relay(_) => true,
        }
    }

    /// The roster party that posted the frame just returned by `recv_text`.
    ///
    /// `None` on the socket, where the gateway authenticated the connection and
    /// every frame on it was implicitly from that peer. On the relay any
    /// approved Companion can post to the same mailbox, so this is what lets
    /// the session tell one from another.
    pub(super) fn last_inbound_party(&self) -> Option<&str> {
        match self {
            Self::WebSocket(_) => None,
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.last_inbound_party(),
        }
    }

    /// Server-authenticated admission subject for the frame just returned.
    /// Socket gateway traffic already carries an authenticated connection
    /// identity and therefore has no relay envelope subject.
    pub(super) fn last_inbound_subject(&self) -> Option<&str> {
        match self {
            Self::WebSocket(_) => None,
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.last_inbound_subject(),
        }
    }

    /// Server-authenticated admission grants for the frame just returned.
    ///
    /// The socket gateway remains unchanged: it is the authority and exposes
    /// no relay envelope metadata, so this is `None` there.
    pub(super) fn last_inbound_grants(&self) -> Option<&HashSet<Grant>> {
        match self {
            Self::WebSocket(_) => None,
            #[cfg(feature = "voice")]
            Self::Relay(channel) => Some(channel.last_inbound_grants()),
        }
    }

    /// Bind one relay device to the party its signed hello proved.
    pub(super) fn authorize_relay_route(
        &mut self,
        device_id: &str,
        party: &str,
        authenticated_grants: &HashSet<Grant>,
    ) {
        match self {
            Self::WebSocket(_) => {}
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.authorize_route(device_id, party, authenticated_grants),
        }
        let _ = (device_id, party, authenticated_grants);
    }

    pub(super) fn narrow_relay_route_grants(
        &mut self,
        device_id: &str,
        party: &str,
        authenticated_grants: &HashSet<Grant>,
    ) {
        match self {
            Self::WebSocket(_) => {}
            #[cfg(feature = "voice")]
            Self::Relay(channel) => {
                channel.narrow_route_grants(device_id, party, authenticated_grants)
            }
        }
        let _ = (device_id, party, authenticated_grants);
    }

    /// Clone-only input for a non-blocking relay greeting refresh.
    #[cfg(feature = "voice")]
    pub(super) fn regreeting_request(&self) -> Option<crate::companion_relay::RelayGreetingRequest> {
        match self {
            Self::WebSocket(_) => None,
            Self::Relay(channel) => Some(channel.regreeting_request()),
        }
    }

    /// Install only into the relay channel that launched the refresh. An
    /// admission rotation replaces that channel and makes late work a no-op.
    #[cfg(feature = "voice")]
    pub(super) fn install_regreeting(&mut self, greeting: FreshRelayGreeting) -> bool {
        match self {
            Self::WebSocket(_) => false,
            Self::Relay(channel) => channel.install_regreeting(
                greeting.channel_id,
                &greeting.party,
                greeting.encoded_hello,
            ),
        }
    }

    /// Preserve carrier-level continuity across an admission rotation.
    ///
    /// The socket needs nothing here — the gateway holds the routing and the
    /// predecessor stays live during the overlap. The relay has no such
    /// middleman: its replacement must inherit the read cursor and the learned
    /// device routes, or it re-reads frames the session already handled.
    pub(super) fn admission_domain(&self) -> Result<AdmissionCarrierDomain, WorkerError> {
        match self {
            Self::WebSocket(_) => Ok(AdmissionCarrierDomain::WebSocket),
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel
                .cursor_domain()
                .map(AdmissionCarrierDomain::Relay)
                .ok_or_else(|| {
                    WorkerError::rebootstrap("Companion relay cursor domain is invalid")
                }),
        }
    }

    /// Commit a prepared replacement's identity proof. Relay hellos are
    /// cached and naturally go out with the next addressed post. WebSocket
    /// hellos fence the predecessor immediately, so they are deliberately
    /// withheld until this call runs on the authority loop immediately before
    /// the atomic transport swap.
    pub(super) async fn activate_replacement(&mut self) -> Result<(), WorkerError> {
        match self {
            Self::WebSocket(transport) => {
                let Some(encoded) = transport.pending_hello.as_ref() else {
                    return Ok(());
                };
                transport
                    .socket
                    .send(Message::Text(encoded.clone().into()))
                    .await
                    .map_err(safe_ws_error)?;
                transport.pending_hello = None;
                Ok(())
            }
            #[cfg(feature = "voice")]
            Self::Relay(_) => Ok(()),
        }
    }

    pub(super) fn adopt_routing_from(&mut self, previous: &mut Self) -> bool {
        #[cfg(feature = "voice")]
        {
            return match (self, previous) {
                (Self::Relay(next), Self::Relay(previous)) => next.adopt_routing_from(previous),
                (Self::WebSocket(_), Self::WebSocket(_)) => true,
                _ => false,
            };
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = (self, previous);
            true
        }
    }

    pub(super) async fn close(self) {
        match self {
            Self::WebSocket(mut transport) => {
                let _ = transport.socket.send(Message::Close(None)).await;
            }
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.close().await,
        }
    }
}

impl WebSocketTransport {
    pub(super) async fn recv_text(&mut self, tick: Duration) -> Result<Option<String>, WorkerError> {
        match tokio::time::timeout(tick, self.socket.next()).await {
            Err(_) => Ok(None),
            Ok(Some(Ok(Message::Text(encoded)))) => Ok(Some(encoded.as_str().to_string())),
            Ok(Some(Ok(Message::Ping(payload)))) => {
                self.socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(safe_ws_error)?;
                Ok(None)
            }
            Ok(Some(Ok(Message::Pong(_)))) => {
                self.awaiting_pong = None;
                Ok(None)
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => Err(WorkerError::reconnect(
                "Companion gateway closed the socket",
            )),
            Ok(Some(Ok(_))) => Err(WorkerError::reconnect(
                "Companion gateway sent a non-text protocol frame",
            )),
            Ok(Some(Err(error))) => Err(safe_ws_error(error)),
        }
    }

    pub(super) async fn tick_heartbeat(&mut self) -> Result<(), WorkerError> {
        if let Some(sent_at) = self.awaiting_pong {
            if sent_at.elapsed() >= PONG_TIMEOUT {
                return Err(WorkerError::reconnect(
                    "Companion gateway heartbeat timed out",
                ));
            }
        }
        if Instant::now() >= self.next_ping {
            self.socket
                .send(Message::Ping(Default::default()))
                .await
                .map_err(safe_ws_error)?;
            self.awaiting_pong = Some(Instant::now());
            self.next_ping = Instant::now() + PING_INTERVAL;
        }
        Ok(())
    }
}

pub(super) async fn open_gateway_socket(
    credentials: &SessionCredentials,
    status: &Arc<Mutex<GatewayStatusSnapshot>>,
    attempt: u32,
    defer_hello: bool,
) -> Result<(GatewaySocket, String, Option<String>), WorkerError> {
    let mut request = credentials
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| WorkerError::rebootstrap("Companion gateway URL cannot form a request"))?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", credentials.token))
        .map_err(|_| WorkerError::rebootstrap("Companion admission token is invalid"))?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("authorization", authorization);
    request.headers_mut().insert(
        "x-aokie-app-id",
        HeaderValue::from_str(&credentials.app_id)
            .map_err(|_| WorkerError::rebootstrap("Companion app identity is invalid"))?,
    );
    let plugin_header = HeaderValue::from_str(&credentials.plugin_id)
        .map_err(|_| WorkerError::rebootstrap("Companion plugin identity is invalid"))?;
    request
        .headers_mut()
        .insert("x-aokie-device-id", plugin_header.clone());
    request
        .headers_mut()
        .insert("x-aokie-plugin-id", plugin_header);

    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(safe_ws_error)?;
    let challenge_encoded = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .map_err(|_| WorkerError::reconnect("Companion endpoint challenge timed out"))?
        .ok_or_else(|| WorkerError::reconnect("Companion gateway closed before challenge"))?
        .map_err(safe_ws_error)?;
    let Message::Text(challenge_encoded) = challenge_encoded else {
        return Err(WorkerError::reconnect(
            "Companion gateway did not send a text endpoint challenge",
        ));
    };
    let challenge: EndpointChallengeFrame = serde_json::from_str(challenge_encoded.as_str())
        .map_err(|_| WorkerError::reconnect("Companion endpoint challenge is malformed"))?;
    let now = unix_now()?;
    let (hello, session_nonce) = endpoint_hello(&challenge, credentials, now)?;
    let pending_hello = if defer_hello {
        Some(
            serde_json::to_string(&hello)
                .map_err(|_| WorkerError::reconnect("Companion frame could not be encoded"))?,
        )
    } else {
        send_json(&mut socket, &hello).await?;
        set_status(status, GatewayConnectionPhase::Connected, attempt, None);
        None
    };
    Ok((socket, session_nonce, pending_hello))
}

/// Validate an endpoint challenge against local identity and sign the plugin
/// hello it demands. Transport-free on purpose: the WebSocket gateway reads
/// its challenge off the socket and the hosted relay fetches the same document
/// over HTTP, and both must produce a byte-identical proof from it.
pub(super) fn endpoint_hello(
    challenge: &EndpointChallengeFrame,
    credentials: &SessionCredentials,
    now: u64,
) -> Result<(PluginHello, String), WorkerError> {
    let session_nonce = format!("plugin_session_{}", uuid::Uuid::new_v4().simple());
    let hello = endpoint_hello_for_session(challenge, credentials, now, &session_nonce)?;
    Ok((hello, session_nonce))
}

/// Mint a fresh endpoint proof while retaining the logical plugin session.
///
/// Relay peers can re-introduce themselves long after the original hello's
/// short proof expired. Re-greeting must bind a fresh relay challenge to the
/// session nonce already carried by leases and RTC authentication; rotating
/// that nonce would instead fence the live session we are trying to preserve.
pub(super) fn endpoint_hello_for_session(
    challenge: &EndpointChallengeFrame,
    credentials: &SessionCredentials,
    now: u64,
    session_nonce: &str,
) -> Result<PluginHello, WorkerError> {
    challenge
        .validate(now)
        .map_err(|_| WorkerError::reconnect("Companion endpoint challenge is invalid"))?;
    let authority = &credentials.endpoint_authority;
    if challenge.app_id != credentials.app_id
        || challenge.subject_id != credentials.plugin_id
        || challenge.role != AdmissionRole::Plugin
        || challenge.holder_key_thumbprint != authority.endpoint_key.thumbprint
        || challenge.expected_peer_key_thumbprint.is_some()
        || challenge.approved_peer_key_thumbprints != authority.approved_thumbprints()
        || challenge.peer_roster_revision != Some(authority.roster_revision)
        || challenge.peer_roster_hash.as_deref() != Some(authority.roster_hash.as_str())
    {
        return Err(WorkerError::rebootstrap(
            "Companion endpoint challenge does not match the local identity and approved roster",
        ));
    }
    let proof_claims = HelloProofClaims {
        app_id: credentials.app_id.clone(),
        subject_id: credentials.plugin_id.clone(),
        role: AdmissionRole::Plugin,
        connection_id: challenge.connection_id.clone(),
        challenge_nonce: challenge.challenge_nonce.clone(),
        admission_jti: challenge.admission_jti.clone(),
        session_nonce: session_nonce.to_owned(),
        holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: authority.approved_thumbprints(),
        peer_roster_revision: Some(authority.roster_revision),
        peer_roster_hash: Some(authority.roster_hash.clone()),
        nonce: format!("proof_nonce_{}", uuid::Uuid::new_v4().simple()),
        jti: format!("proof_jti_{}", uuid::Uuid::new_v4().simple()),
        issued_at: now,
        expires_at: challenge.expires_at.min(now.saturating_add(30)),
    };
    proof_claims
        .validate(now)
        .map_err(|_| WorkerError::rebootstrap("Companion endpoint proof claims are invalid"))?;
    let proof = SignedHelloProof {
        endpoint_key: authority.endpoint_key.clone(),
        signature: authority.sign(&proof_claims.signing_bytes().map_err(|_| {
            WorkerError::rebootstrap("Companion endpoint proof could not be canonicalized")
        })?),
        claims: proof_claims,
    };
    let hello = PluginHello {
        kind: "plugin_hello".into(),
        schema_version: SCHEMA_VERSION,
        app_id: credentials.app_id.clone(),
        plugin_id: credentials.plugin_id.clone(),
        session_nonce: session_nonce.to_owned(),
        endpoint_proof: proof,
    };
    hello
        .validate()
        .map_err(|_| WorkerError::rebootstrap("Companion plugin hello is invalid"))?;
    Ok(hello)
}

pub(super) fn admission_rotation_times(now: Instant, lifetime: Duration) -> (Instant, Instant) {
    let deadline = now + lifetime;
    let overlap = lifetime.min(ADMISSION_ROTATION_OVERLAP);
    let starts_at = deadline.checked_sub(overlap).unwrap_or(now);
    (starts_at, deadline)
}

pub(super) async fn run_socket(
    mut credentials: SessionCredentials,
    host_rpc: &Arc<HostRpc>,
    radio: &RadioHandle,
    stop_rx: &mut watch::Receiver<bool>,
    status: &Arc<Mutex<GatewayStatusSnapshot>>,
    attempt: u32,
) -> Result<(), WorkerError> {
    let (mut transport, session_nonce) =
        GatewayTransport::open(&credentials, status, attempt).await?;
    let media = radio
        .remote_media()
        .ok_or_else(|| WorkerError::reconnect("Companion media endpoint is unavailable"))?;
    let mut session = GatewaySession::new(&credentials, session_nonce);
    let (mut admission_refresh_at, mut admission_deadline) =
        admission_rotation_times(Instant::now(), credentials.lifetime);
    let mut admission_retry_at = admission_refresh_at;
    let mut admission_generation = 0_u64;
    let mut admission_rotation: Option<AdmissionRotationTask> = None;
    let mut admission_rotation_error: Option<WorkerError> = None;
    let mut retiring_transport: Option<(GatewayTransport, Instant)> = None;
    #[cfg(feature = "voice")]
    let mut relay_greeting_tasks = RelayGreetingTasks::default();

    loop {
        if retiring_transport
            .as_ref()
            .is_some_and(|(_, deadline)| Instant::now() >= *deadline)
        {
            // Dropped rather than closed, exactly as before: the gateway has
            // already fenced this predecessor off the replacement hello, and
            // an explicit close frame here would be new behaviour on the
            // rotation path a live call depends on.
            retiring_transport.take();
        }
        if *stop_rx.borrow() {
            transport.close().await;
            if let Some((retiring, _)) = retiring_transport.take() {
                retiring.close().await;
            }
            return Ok(());
        }
        let rotation_result = match admission_rotation.as_mut() {
            Some(rotation) => rotation.take_finished().await,
            None => None,
        };
        if let Some(result) = rotation_result {
            admission_rotation.take();
            match result {
                Ok(opened) if opened.generation == admission_generation => {
                    let OpenedAdmissionRotation {
                        credentials: refreshed,
                        transport: mut replacement,
                        plugin_session_nonce: replacement_nonce,
                        preserve_continuity,
                        ..
                    } = opened;

                    // A WebSocket plugin_hello fences the predecessor at the
                    // gateway. Commit it only now, when this loop already owns
                    // the finished replacement and will not read/write the old
                    // socket again before swapping.
                    replacement.activate_replacement().await?;
                    session.apply_admission_rotation(
                        &refreshed,
                        replacement_nonce,
                        preserve_continuity,
                        media,
                    )?;

                    // Keep the authenticated predecessor alive briefly while
                    // the gateway consumes the replacement hello. The
                    // predecessor has continued reading and renewing leases
                    // for the whole broker/open overlap; only this atomic swap
                    // transfers its cursor and routes to the proven successor.
                    let mut predecessor = std::mem::replace(&mut transport, replacement);
                    if preserve_continuity && !transport.adopt_routing_from(&mut predecessor) {
                        media.fail_closed_all("gateway_admission_continuity_mismatch");
                        return Err(WorkerError::reconnect(
                            "Companion replacement transport changed continuity domains",
                        ));
                    }
                    retiring_transport.take();
                    retiring_transport =
                        Some((predecessor, Instant::now() + Duration::from_secs(2)));
                    #[cfg(feature = "voice")]
                    relay_greeting_tasks.abort_all();
                    credentials = refreshed;
                    admission_generation = admission_generation.saturating_add(1);
                    let times = admission_rotation_times(Instant::now(), credentials.lifetime);
                    admission_refresh_at = times.0;
                    admission_deadline = times.1;
                    admission_retry_at = admission_refresh_at;
                    admission_rotation_error = None;
                    eprintln!(
                        "[aokie-plugin][companion] stage=admission_rotated continuity={} transport={} app={} plugin={} active_peers={}",
                        if preserve_continuity { "preserved" } else { "reset" },
                        credentials.transport_label(),
                        credentials.app_id,
                        credentials.plugin_id,
                        session.peers.len()
                    );
                    continue;
                }
                Ok(opened) => {
                    // A newer generation won before this completion was
                    // observed. It owns no session state; close it and leave
                    // the current carrier authoritative.
                    opened.transport.close().await;
                    admission_rotation_error = Some(WorkerError::reconnect(
                        "A stale Companion admission replacement was discarded",
                    ));
                }
                Err(error) => {
                    eprintln!(
                        "[aokie-plugin][companion] stage=admission_rotation_deferred kind={} detail={}",
                        error.kind.label(),
                        sanitize_status_message(&error.message)
                    );
                    admission_rotation_error = Some(error);
                }
            }
            admission_retry_at = Instant::now() + ADMISSION_ROTATION_RETRY_DELAY;
        }

        let now = Instant::now();
        if now >= admission_deadline {
            admission_rotation.take();
            return Err(admission_rotation_error.take().unwrap_or_else(|| {
                WorkerError::expired(
                    "Companion admission could not rotate before its safe lifetime ended",
                )
            }));
        }
        if admission_rotation.is_none() && now >= admission_refresh_at && now >= admission_retry_at
        {
            let predecessor_domain = transport.admission_domain()?;
            match AdmissionRotationTask::begin(
                Arc::clone(host_rpc),
                admission_generation,
                &credentials,
                Arc::clone(status),
                attempt,
                predecessor_domain,
            ) {
                Ok(rotation) => {
                    admission_rotation = Some(rotation);
                    eprintln!(
                        "[aokie-plugin][companion] stage=admission_rotation_started continuity=overlap transport={} app={} plugin={}",
                        credentials.transport_label(),
                        credentials.app_id,
                        credentials.plugin_id
                    );
                }
                Err(error) => {
                    eprintln!(
                        "[aokie-plugin][companion] stage=admission_rotation_deferred kind={} detail={}",
                        error.kind.label(),
                        sanitize_status_message(&error.message)
                    );
                    admission_rotation_error = Some(error);
                    admission_retry_at = now + ADMISSION_ROTATION_RETRY_DELAY;
                }
            }
        }

        #[cfg(feature = "voice")]
        for completed in relay_greeting_tasks.take_finished().await {
            match completed {
                Ok(greeting) => {
                    if greeting.plugin_session_nonce == session.plugin_session_nonce
                        && transport.install_regreeting(greeting)
                    {
                        // State may already have been published while the HTTP
                        // challenge was in flight. Re-arm it now so the next
                        // publish is guaranteed to follow the fresh hello.
                        session.rearm_authoritative_publication();
                    } else {
                        // A logical-session or channel rotation won the race.
                        // Its own newly armed hello is authoritative; stale
                        // work is a contained no-op.
                    }
                }
                Err(error) => {
                    // A fresh peer greeting is recoverable signalling. Never
                    // turn a challenge outage into a live-call failback; a
                    // later verified mobile hello will schedule another try.
                    eprintln!(
                        "[aokie-plugin][companion] stage=relay_regreet_deferred kind={} detail={}",
                        error.kind.label(),
                        sanitize_status_message(&error.message)
                    );
                }
            }
        }

        // Which carrier is live decides two things the session cannot infer on
        // its own: whether it publishes its own offers, and whether it consumes
        // its own claim decision instead of addressing one to a gateway.
        session.relay_carrier = transport.is_relay();

        // Lease heartbeats prove only that the app process is alive. They do
        // not prove that an ACTIVE replacement peer ever opened, nor can a
        // dropped terminal event be allowed to leave gateway authority alive
        // after the media state has already returned the caller to Aokie.
        session.expire_relay_leases(unix_now()?, media);
        session.expire_unbound_active_rebind(Instant::now(), media);
        session.reconcile_relay_media_authority(media);
        session.reconcile_accepted_transfers(unix_now()?, media, radio);

        let publication_due = Instant::now() >= session.next_snapshot_poll;
        if publication_due {
            session.next_snapshot_poll = Instant::now() + SNAPSHOT_POLL;
            // A terminal revoke is the exact proof that caller authority has
            // returned. Pay the oldest bounded delivery debt before every
            // ordinary egress lane, without re-running completed teardown.
            for encoded in session.due_pending_relay_revocations(Instant::now()) {
                session.prepare_relay_delivery(&encoded);
                let delivery = transport.send_text(&encoded).await?;
                session.finish_relay_delivery(&encoded, delivery, media, radio);
            }
        }
        for encoded in session.drain_end_caller_results(media)? {
            session.prepare_relay_delivery(&encoded);
            let delivery = transport.send_text(&encoded).await?;
            session.finish_relay_delivery(&encoded, delivery, media, radio);
        }
        for encoded in session.drain_media_events(media, radio)? {
            session.prepare_relay_delivery(&encoded);
            let delivery = transport.send_text(&encoded).await?;
            session.finish_relay_delivery(&encoded, delivery, media, radio);
        }
        if publication_due {
            for encoded in session.authoritative_state_frames(radio)? {
                session.prepare_relay_delivery(&encoded);
                let delivery = transport.send_text(&encoded).await?;
                session.finish_relay_delivery(&encoded, delivery, media, radio);
            }
            if let Some(encoded) = session.assistance_frame(radio)? {
                session.prepare_relay_delivery(&encoded);
                let delivery = transport.send_text(&encoded).await?;
                session.finish_relay_delivery(&encoded, delivery, media, radio);
            }
        }

        transport.tick_heartbeat().await?;

        let Some(encoded) = transport.recv_text(READ_TICK).await? else {
            continue;
        };
        let from_relay_peer = transport.is_relay();
        let inbound_party = transport.last_inbound_party().map(str::to_owned);
        let inbound_subject = transport.last_inbound_subject().map(str::to_owned);
        let inbound_grants = transport.last_inbound_grants().cloned();
        let inbound_kind = serde_json::from_str::<Envelope>(&encoded)
            .map(|frame| frame.kind)
            .unwrap_or_else(|_| "malformed".into());
        let outbound = session
            .handle_inbound(
                &encoded,
                media,
                radio,
                from_relay_peer,
                inbound_party.as_deref(),
                inbound_subject.as_deref(),
                inbound_grants.as_ref(),
            )
            .map_err(|error| {
                eprintln!(
                    "[aokie-plugin][companion] stage=inbound_rejected frame={} kind={} detail={}",
                    inbound_kind,
                    error.kind.label(),
                    sanitize_status_message(&error.message)
                );
                error
            })?;
        // Relay admission can narrow on any later authenticated frame. Keep
        // transport-level privacy projections in the same fail-closed state
        // before sending outbound assistance/caller context. Ordinary frames
        // may remove grants; only a newly verified hello may broaden them.
        if let (Some(device_id), Some(party), Some(grants)) = (
            inbound_subject.as_deref(),
            inbound_party.as_deref(),
            inbound_grants.as_ref(),
        ) {
            transport.narrow_relay_route_grants(device_id, party, grants);
        }
        // Route ownership is installed only after the mobile hello's endpoint
        // proof, owner roster membership and actual relay sender all agree.
        // Doing this before the response loop is what makes the very first
        // targeted acceptance/grant reach the device that proved it.
        if let Some((device_id, party)) = session.take_relay_verified_route() {
            transport.authorize_relay_route(
                &device_id,
                &party,
                inbound_grants.as_ref().unwrap_or(&HashSet::new()),
            );
        }
        // Before anything else goes out: a Companion that just proved itself
        // may have lost the proof WE gave it (a restarted process keeps its
        // on-disk endpoint key, so it is the same party, but its memory of our
        // hello is gone). Fetch a new challenge and replace the cached proof
        // before retiring its greeting mark, while the re-armed publish is
        // still one loop turn away, so the state it is about to receive arrives
        // behind a CURRENT hello it can verify.
        if let Some(party) = session.take_relay_regreet_party() {
            #[cfg(feature = "voice")]
            if let Some(request) = transport.regreeting_request() {
                relay_greeting_tasks.schedule(
                    party,
                    request,
                    credentials.clone(),
                    session.plugin_session_nonce.clone(),
                );
            }
            #[cfg(not(feature = "voice"))]
            let _ = party;
        }
        for encoded in outbound {
            session.prepare_relay_delivery(&encoded);
            let delivery = transport.send_text(&encoded).await?;
            session.finish_relay_delivery(&encoded, delivery, media, radio);
        }
    }
}

pub(super) async fn send_json<T: Serialize>(socket: &mut GatewaySocket, value: &T) -> Result<(), WorkerError> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| WorkerError::reconnect("Companion frame could not be encoded"))?;
    socket
        .send(Message::Text(encoded.into()))
        .await
        .map_err(safe_ws_error)
}

pub(super) fn safe_ws_error(error: tungstenite::Error) -> WorkerError {
    let message = match error {
        tungstenite::Error::Http(response) => {
            format!(
                "Companion gateway rejected the connection ({})",
                response.status()
            )
        }
        tungstenite::Error::Io(error) => {
            format!("Companion gateway network error ({:?})", error.kind())
        }
        tungstenite::Error::Tls(_) => "Companion gateway TLS validation failed".into(),
        tungstenite::Error::Capacity(_) => "Companion gateway frame exceeded a safety limit".into(),
        tungstenite::Error::Protocol(_) => "Companion gateway WebSocket protocol failed".into(),
        tungstenite::Error::Url(_) => "Companion gateway URL is unsupported".into(),
        tungstenite::Error::HttpFormat(_) => "Companion gateway request headers are invalid".into(),
        _ => "Companion gateway connection failed".into(),
    };
    WorkerError::reconnect(message)
}
