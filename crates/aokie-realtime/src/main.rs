use aokie_realtime::{Gateway, GatewayConfig};

#[tokio::main]
async fn main() {
    let config = match GatewayConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("aokie-realtime: {error}");
            std::process::exit(2);
        }
    };
    let bind = config.bind;
    let gateway = match Gateway::new(config) {
        Ok(gateway) => gateway,
        Err(error) => {
            eprintln!("aokie-realtime: {error}");
            std::process::exit(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("aokie-realtime: cannot bind {bind}: {error}");
            std::process::exit(1);
        }
    };
    eprintln!("aokie-realtime: listening on http://{bind} (put behind TLS for remote use)");
    if let Err(error) = axum::serve(listener, gateway.router()).await {
        eprintln!("aokie-realtime: server stopped: {error}");
        std::process::exit(1);
    }
}
