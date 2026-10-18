//! TLS for the Companion's WebSockets.
//!
//! HTTPS (`reqwest`) checks server certificates against the platform's trust
//! store through `rustls-platform-verifier`, but tokio-tungstenite's own TLS
//! uses a bundled copy of the public web roots. On Android that meant a
//! self-hosted gateway whose certificate the phone trusts (a company or home
//! CA installed on the device) passed every HTTPS step and then failed the
//! WSS connect. On Android the sockets now use the same platform verifier as
//! HTTPS; elsewhere they keep tokio-tungstenite's default.

use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest, handshake::client::Response, protocol::WebSocketConfig,
        Error as WebSocketError,
    },
    Connector, MaybeTlsStream, WebSocketStream,
};

/// `connect_async_with_config` with this module's TLS choice.
pub(crate) async fn connect<R>(
    request: R,
    config: WebSocketConfig,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), WebSocketError>
where
    R: IntoClientRequest + Unpin,
{
    let connector = connector().map_err(|message| {
        WebSocketError::Io(std::io::Error::other(message))
    })?;
    connect_async_tls_with_config(request, Some(config), false, connector).await
}

/// The TLS connector a Companion WebSocket uses: the platform verifier on
/// Android, tokio-tungstenite's default (`None`) elsewhere.
fn connector() -> Result<Option<Connector>, String> {
    #[cfg(target_os = "android")]
    {
        Ok(Some(Connector::Rustls(platform_client_config()?)))
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(None)
    }
}

/// A rustls client configuration that verifies certificates with the
/// platform's trust store, on the same aws-lc-rs provider `reqwest` uses.
/// On Android it refuses (rather than letting the first handshake panic)
/// until `native_init` has handed the verifier its Context.
#[cfg(any(target_os = "android", test))]
fn platform_client_config() -> Result<std::sync::Arc<rustls::ClientConfig>, String> {
    use rustls_platform_verifier::BuilderVerifierExt;

    crate::native_init::ensure_https_ready()?;
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("WebSocket TLS versions are unavailable: {error}"))?
        .with_platform_verifier()
        .map_err(|error| format!("the platform certificate verifier is unavailable: {error}"))?
        .with_no_client_auth();
    Ok(std::sync::Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_verifier_config_builds_with_an_explicit_provider() {
        // An implicit `ClientConfig::builder()` panics when more than one
        // rustls provider is compiled in, as both ring and aws-lc-rs are in
        // this graph; the explicit provider keeps that from ever happening.
        let config = platform_client_config().expect("platform verifier config");
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn off_android_the_socket_keeps_the_default_tls() {
        if cfg!(not(target_os = "android")) {
            assert!(connector().unwrap().is_none());
        }
    }
}
