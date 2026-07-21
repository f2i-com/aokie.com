//! Worker error taxonomy and the admission retry schedule.

#[allow(unused_imports)]
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerErrorKind {
    Reconnect,
    Expired,
    AdmissionRefresh,
    Rebootstrap,
}

/// Whether a carrier actually accepted a frame for delivery.
///
/// `Dropped` is deliberately non-fatal: relay backpressure must not tear down
/// a live call. It is still distinct from success because a consult/takeover
/// claim may only be armed after its provisional grant was accepted by the
/// carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportDelivery {
    Delivered,
    Dropped,
}

impl WorkerErrorKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Reconnect => "reconnect",
            Self::Expired => "expired",
            Self::AdmissionRefresh => "admission_refresh",
            Self::Rebootstrap => "rebootstrap",
        }
    }
}

#[derive(Debug)]
pub(crate) struct WorkerError {
    pub(crate) kind: WorkerErrorKind,
    pub(crate) message: String,
}

impl WorkerError {
    pub(crate) fn reconnect(message: impl Into<String>) -> Self {
        Self {
            kind: WorkerErrorKind::Reconnect,
            message: message.into(),
        }
    }

    pub(super) fn expired(message: impl Into<String>) -> Self {
        Self {
            kind: WorkerErrorKind::Expired,
            message: message.into(),
        }
    }

    pub(crate) fn rebootstrap(message: impl Into<String>) -> Self {
        Self {
            kind: WorkerErrorKind::Rebootstrap,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RetrySchedule {
    pub(super) phase: GatewayConnectionPhase,
    pub(super) attempt: u32,
    pub(super) delay: Duration,
}

pub(super) fn retry_schedule(
    kind: WorkerErrorKind,
    prior_attempt: u32,
    had_connected_session: bool,
) -> RetrySchedule {
    if kind == WorkerErrorKind::AdmissionRefresh {
        return RetrySchedule {
            phase: GatewayConnectionPhase::AdmissionRefresh,
            attempt: 0,
            delay: Duration::ZERO,
        };
    }

    // A completed endpoint handshake proves the previous failure streak has
    // recovered.  Do not carry stale backoff across a healthy 80-second
    // admission window (or any other established session).
    let base_attempt = if had_connected_session {
        0
    } else {
        prior_attempt
    };
    let attempt = base_attempt.saturating_add(1);
    let exponent = attempt.saturating_sub(1).min(5);
    RetrySchedule {
        phase: match kind {
            WorkerErrorKind::Reconnect => GatewayConnectionPhase::Reconnecting,
            WorkerErrorKind::Expired => GatewayConnectionPhase::Expired,
            WorkerErrorKind::AdmissionRefresh => GatewayConnectionPhase::AdmissionRefresh,
            WorkerErrorKind::Rebootstrap => GatewayConnectionPhase::RebootstrapRequired,
        },
        attempt,
        delay: Duration::from_secs(1_u64 << exponent).min(MAX_BACKOFF),
    }
}

pub(super) async fn gateway_worker(
    startup: GatewayStartup,
    radio: RadioHandle,
    host_rpc: Arc<HostRpc>,
    mut stop_rx: watch::Receiver<bool>,
    status: Arc<Mutex<GatewayStatusSnapshot>>,
) {
    let (mut app_id, plugin_id, endpoint_authority, mut initial) = match startup {
        GatewayStartup::Managed {
            app_id,
            plugin_id,
            endpoint_authority,
            initial,
        } => (app_id, plugin_id, endpoint_authority, initial),
    };
    let mut attempt = 0_u32;

    loop {
        if *stop_rx.borrow() {
            break;
        }
        let credentials = if let Some(credentials) = initial.take() {
            set_status(&status, GatewayConnectionPhase::Connecting, attempt, None);
            Ok(credentials)
        } else {
            set_status(
                &status,
                GatewayConnectionPhase::AdmissionRefresh,
                attempt,
                None,
            );
            refresh_admission(
                &host_rpc,
                app_id.as_deref(),
                &plugin_id,
                endpoint_authority.clone(),
            )
        };

        let outcome = match credentials {
            Ok(credentials) => {
                app_id = Some(credentials.app_id.clone());
                run_socket(
                    credentials,
                    &host_rpc,
                    &radio,
                    &mut stop_rx,
                    &status,
                    attempt,
                )
                .await
            }
            Err(error) => Err(error),
        };
        if *stop_rx.borrow() {
            break;
        }

        let had_connected_session = status
            .lock()
            .map(|status| status.connected)
            .unwrap_or(false);
        let error = outcome
            .err()
            .unwrap_or_else(|| WorkerError::reconnect("Companion gateway disconnected"));
        eprintln!(
            "[aokie-plugin][companion] stage=socket_ended kind={} detail={}",
            error.kind.label(),
            sanitize_status_message(&error.message)
        );
        radio
            .remote_media()
            .inspect(|media| media.fail_closed_all("gateway_disconnected"));
        let retry = retry_schedule(error.kind, attempt, had_connected_session);
        attempt = retry.attempt;
        set_status(&status, retry.phase, attempt, Some(error.message));
        if retry.delay.is_zero() {
            continue;
        }

        let sleep = tokio::time::sleep(retry.delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        set_status(&status, GatewayConnectionPhase::Stopped, attempt, None);
                        return;
                    }
                }
            }
        }
    }

    if let Some(media) = radio.remote_media() {
        media.fail_closed_all("gateway_stopped");
    }
    set_status(&status, GatewayConnectionPhase::Stopped, attempt, None);
}

pub(super) fn refresh_admission(
    host_rpc: &HostRpc,
    app_id: Option<&str>,
    plugin_id: &str,
    endpoint_authority: Arc<EndpointAuthority>,
) -> Result<SessionCredentials, WorkerError> {
    let params = admission_request_params(app_id, plugin_id, &endpoint_authority)?;
    let (request_id, line, receiver) = host_rpc.begin("companion.admission", Value::Object(params));
    let mut sink = StdoutSink::new();
    if sink.send_line(&line).is_err() {
        host_rpc.forget(request_id);
        return Err(WorkerError::rebootstrap(
            "Desktop admission broker is unavailable",
        ));
    }
    let value = match receiver.recv_timeout(ADMISSION_RPC_TIMEOUT) {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => {
            return Err(WorkerError::rebootstrap(
                "Desktop rejected Companion admission refresh",
            ))
        }
        Err(_) => {
            host_rpc.forget(request_id);
            return Err(WorkerError::rebootstrap(
                "Desktop admission refresh timed out",
            ));
        }
    };
    let response: AdmissionResponse = serde_json::from_value(value).map_err(|_| {
        WorkerError::rebootstrap("Desktop returned an invalid Companion admission response")
    })?;
    response.into_credentials(app_id, plugin_id, endpoint_authority)
}

/// The `companion.admission` RPC request. Pure so the transport negotiation
/// below can be locked in a test — Desktop strips the relay advertisement from
/// every admission until the plugin asks for it, so this request IS the switch
/// that activates the hosted carrier.
pub(super) fn admission_request_params(
    app_id: Option<&str>,
    plugin_id: &str,
    endpoint_authority: &EndpointAuthority,
) -> Result<serde_json::Map<String, Value>, WorkerError> {
    let mut params = serde_json::Map::new();
    if let Some(app_id) = app_id {
        params.insert("appId".into(), Value::String(app_id.into()));
    }
    params.insert("pluginId".into(), Value::String(plugin_id.into()));
    params.insert("displayName".into(), Value::String("Aokie Desktop".into()));
    params.insert(
        "endpointPublicKey".into(),
        serde_json::to_value(&endpoint_authority.endpoint_key)
            .map_err(|_| WorkerError::rebootstrap("Endpoint public key could not be encoded"))?,
    );
    params.insert(
        "holderKeyThumbprint".into(),
        Value::String(endpoint_authority.endpoint_key.thumbprint.clone()),
    );
    params.insert(
        "approvedPeerKeyThumbprints".into(),
        serde_json::to_value(endpoint_authority.approved_thumbprints())
            .map_err(|_| WorkerError::rebootstrap("Endpoint roster could not be encoded"))?,
    );
    params.insert(
        "peerRosterRevision".into(),
        Value::from(endpoint_authority.roster_revision),
    );
    params.insert(
        "peerRosterHash".into(),
        Value::String(endpoint_authority.roster_hash.clone()),
    );
    // Desktop NEGOTIATES the optional `relay` member rather than deploy-ordering
    // it: it forwards the advertisement only to a build that asks, so a plugin
    // that predates the transport keeps the exact pre-relay wire shape whichever
    // side upgrades first. Asking is therefore what activates the carrier — the
    // member is stripped from every admission until this is sent.
    //
    // Gated with the carrier itself: a non-voice build cannot open a relay
    // channel, so it must not ask for endpoints it would only log and ignore.
    if cfg!(feature = "voice") {
        params.insert(
            "supportedTransports".into(),
            Value::Array(vec![Value::String(RELAY_TRANSPORT.into())]),
        );
    }
    Ok(params)
}
