//! `CompanionGatewayHandle`: the plugin-side lifecycle handle.

#[allow(unused_imports)]
use super::*;

pub struct CompanionGatewayHandle {
    pub(super) stop_tx: watch::Sender<bool>,
    pub(super) status: Arc<Mutex<GatewayStatusSnapshot>>,
}

impl CompanionGatewayHandle {
    pub fn spawn(
        bootstrap: CompanionBootstrap,
        radio: RadioHandle,
        host_rpc: Arc<HostRpc>,
    ) -> Result<Self, String> {
        let startup = GatewayStartup::from_bootstrap(bootstrap)?;
        Self::spawn_start(startup, radio, host_rpc, GatewayConnectionPhase::Connecting)
    }

    pub(super) fn spawn_start(
        startup: GatewayStartup,
        radio: RadioHandle,
        host_rpc: Arc<HostRpc>,
        phase: GatewayConnectionPhase,
    ) -> Result<Self, String> {
        let mut initial_status = GatewayStatusSnapshot::starting();
        initial_status.phase = phase;
        let status = Arc::new(Mutex::new(initial_status));
        let (stop_tx, stop_rx) = watch::channel(false);
        let worker_status = status.clone();
        std::thread::Builder::new()
            .name("aokie-companion-gateway".into())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        set_status(
                            &worker_status,
                            GatewayConnectionPhase::RebootstrapRequired,
                            0,
                            Some("Companion gateway runtime could not start".into()),
                        );
                        return;
                    }
                };
                runtime.block_on(gateway_worker(
                    startup,
                    radio,
                    host_rpc,
                    stop_rx,
                    worker_status,
                ));
            })
            .map_err(|error| format!("start Companion gateway worker: {error}"))?;
        Ok(Self { stop_tx, status })
    }

    pub fn status(&self) -> GatewayStatusSnapshot {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| GatewayStatusSnapshot {
                configured: true,
                connected: false,
                phase: GatewayConnectionPhase::RebootstrapRequired,
                reconnect_attempt: 0,
                last_error: Some("Companion gateway status is unavailable".into()),
                changed_at: aokie_core::events::now_iso8601(),
            })
    }

    pub fn stop(&self) {
        self.stop_tx.send_replace(true);
    }
}

pub(super) enum GatewayStartup {
    Managed {
        app_id: Option<String>,
        plugin_id: String,
        endpoint_authority: Arc<EndpointAuthority>,
        initial: Option<SessionCredentials>,
    },
}

impl GatewayStartup {
    pub(super) fn from_bootstrap(bootstrap: CompanionBootstrap) -> Result<Self, String> {
        bootstrap.validate()?;
        let endpoint_authority = Arc::new(EndpointAuthority::from_bootstrap(
            &bootstrap.endpoint_identity,
            &bootstrap.approved_mobile_roster,
        )?);
        let initial = SessionCredentials::initial(&bootstrap, endpoint_authority.clone())?;
        Ok(Self::Managed {
            app_id: bootstrap.app_id.clone(),
            plugin_id: bootstrap.plugin_id.clone(),
            endpoint_authority,
            initial,
        })
    }
}

impl Drop for CompanionGatewayHandle {
    fn drop(&mut self) {
        self.stop();
    }
}
