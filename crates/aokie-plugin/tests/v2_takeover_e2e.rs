//! Cross-component protocol-v2 release harness.
//!
//! The routine test starts the real gateway on an ephemeral socket and drives
//! distinct HMAC-authenticated plugin/mobile WebSockets.  It proves the
//! authoritative snapshot, monitor lease, two-phase takeover, authority-token
//! rotation, fresh active SDP generation, revocation, and stale-JTI rejection.
//!
//! The ignored test adds the operating-system audio boundary.  It uses the
//! plugin's real `RemoteMediaHandle` (and therefore a real `DesktopPeer`) plus
//! the shared native `CompanionPeer`.  Run it on a release workstation with:
//!
//! ```text
//! cargo test -p aokie-plugin --test v2_takeover_e2e native_ -- --ignored --nocapture
//! ```

use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aokie_media::{CompanionPeer, MediaMode, PeerEvent, PeerOptions, RoutePermit, SessionBinding};
use aokie_plugin::remote_media::{
    OpenPeerRequest, RadioTransition, RemoteConsentGate, RemoteMediaEventKind, RemoteMediaHandle,
    ServiceMode, REMOTE_CONSENT_POLICY_ID,
};
use aokie_protocol::v2::{
    peer_roster_hash, sdp_dtls_fingerprint, sdp_sha256, AdmissionClaims, AdmissionRole,
    EndpointBindingClaims, EndpointChallengeFrame, EndpointPublicKey, Grant, HelloProofClaims,
    LeaseClaims, LeaseMode, LeasePhase, MediaTrack, SignedEndpointBinding, SignedHelloProof,
    ADMISSION_AUDIENCE,
};
use aokie_realtime::{v2::AdmissionTokenSigner, Gateway, GatewayConfig};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer as _, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{net::TcpStream, task::JoinHandle};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, protocol::Message},
    MaybeTlsStream, WebSocketStream,
};

const APP_ID: &str = "app_e2e";
const PLUGIN_ID: &str = "plugin_e2e";
const DEVICE_ID: &str = "device_e2e";
const CALL_ID: &str = "call_e2e";
const CALL_EPOCH: u64 = 1;
const ADMISSION_SECRET: &[u8] = b"e2e-admission-secret-0123456789abcdef";
const LEASE_SECRET: &str = "e2e-lease-secret-0123456789abcdef";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct TestGateway {
    ws_url: String,
    task: JoinHandle<()>,
    plugin_token: String,
    mobile_token: String,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("valid system clock")
        .as_secs()
}

fn full_remote_consent() -> RemoteConsentGate {
    RemoteConsentGate {
        policy_id: REMOTE_CONSENT_POLICY_ID.into(),
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
    }
}

fn environment_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn plugin_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[31; 32])
}

fn mobile_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[41; 32])
}

fn endpoint_key(signing_key: &SigningKey) -> EndpointPublicKey {
    EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes())
}

fn issue_admissions() -> (String, String) {
    let now = unix_now();
    let signer = AdmissionTokenSigner::new(ADMISSION_SECRET.to_vec()).expect("admission signer");
    let plugin_key = endpoint_key(&plugin_signing_key());
    let mobile_key = endpoint_key(&mobile_signing_key());
    let approved_mobile_keys = vec![mobile_key.thumbprint.clone()];
    let plugin_claims = AdmissionClaims {
        aud: ADMISSION_AUDIENCE.into(),
        app_id: APP_ID.into(),
        subject_id: PLUGIN_ID.into(),
        role: AdmissionRole::Plugin,
        holder_key_thumbprint: plugin_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: approved_mobile_keys.clone(),
        peer_roster_revision: Some(1),
        peer_roster_hash: Some(peer_roster_hash(1, &approved_mobile_keys)),
        scopes: vec![Grant::StateRead, Grant::RtcSignal],
        exp: now + 120,
        jti: format!("admission_plugin_{}", uuid::Uuid::new_v4().simple()),
    };
    let mobile_claims = AdmissionClaims {
        aud: ADMISSION_AUDIENCE.into(),
        app_id: APP_ID.into(),
        subject_id: DEVICE_ID.into(),
        role: AdmissionRole::Mobile,
        holder_key_thumbprint: mobile_key.thumbprint,
        expected_peer_key_thumbprint: Some(plugin_key.thumbprint),
        approved_peer_key_thumbprints: vec![],
        peer_roster_revision: None,
        peer_roster_hash: None,
        scopes: vec![
            Grant::StateRead,
            Grant::CallerRead,
            Grant::CaptionsRead,
            Grant::AssistanceRead,
            Grant::AssistanceRespond,
            Grant::Monitor,
            Grant::Consult,
            Grant::Takeover,
            Grant::ResumeAokie,
            Grant::RtcSignal,
        ],
        exp: now + 120,
        jti: format!("admission_mobile_{}", uuid::Uuid::new_v4().simple()),
    };
    let plugin = signer.issue(&plugin_claims, now).expect("plugin token");
    let mobile = signer.issue(&mobile_claims, now).expect("mobile token");
    assert_ne!(
        plugin, mobile,
        "the two roles must have distinct admissions"
    );
    assert_eq!(signer.verify(&plugin, now).unwrap(), plugin_claims);
    assert_eq!(signer.verify(&mobile, now).unwrap(), mobile_claims);
    (plugin, mobile)
}

async fn start_gateway() -> TestGateway {
    // `GatewayConfig::from_env` is the production configuration parser.  This
    // integration-test binary contains one test at a time, and the lock also
    // documents that process environment mutation must remain serialized.
    let _guard = environment_lock().lock().expect("environment lock");
    let previous = [
        ("AOKIE_GATEWAY_BIND", std::env::var_os("AOKIE_GATEWAY_BIND")),
        (
            "AOKIE_GATEWAY_ADMISSIONS",
            std::env::var_os("AOKIE_GATEWAY_ADMISSIONS"),
        ),
        (
            "AOKIE_GATEWAY_ADMISSION_HMAC_SECRET",
            std::env::var_os("AOKIE_GATEWAY_ADMISSION_HMAC_SECRET"),
        ),
        (
            "AOKIE_GATEWAY_LEASE_HMAC_SECRET",
            std::env::var_os("AOKIE_GATEWAY_LEASE_HMAC_SECRET"),
        ),
        (
            "AOKIE_GATEWAY_V2_ALLOW_STATIC_ADMISSIONS",
            std::env::var_os("AOKIE_GATEWAY_V2_ALLOW_STATIC_ADMISSIONS"),
        ),
    ];
    std::env::set_var("AOKIE_GATEWAY_BIND", "127.0.0.1:0");
    std::env::remove_var("AOKIE_GATEWAY_ADMISSIONS");
    std::env::set_var(
        "AOKIE_GATEWAY_ADMISSION_HMAC_SECRET",
        std::str::from_utf8(ADMISSION_SECRET).unwrap(),
    );
    std::env::set_var("AOKIE_GATEWAY_LEASE_HMAC_SECRET", LEASE_SECRET);
    std::env::remove_var("AOKIE_GATEWAY_V2_ALLOW_STATIC_ADMISSIONS");
    let config = GatewayConfig::from_env().expect("dynamic v2 gateway config");
    for (name, value) in previous {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    drop(_guard);

    let gateway = Gateway::new(config).expect("gateway");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        axum::serve(listener, gateway.router())
            .await
            .expect("gateway server");
    });
    let (plugin_token, mobile_token) = issue_admissions();
    TestGateway {
        ws_url: format!("ws://{address}/v2/realtime"),
        task,
        plugin_token,
        mobile_token,
    }
}

async fn connect(gateway: &TestGateway, subject: &str, token: &str) -> Socket {
    let mut request = gateway
        .ws_url
        .clone()
        .into_client_request()
        .expect("WebSocket request");
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    request
        .headers_mut()
        .insert("x-aokie-app-id", HeaderValue::from_static(APP_ID));
    request
        .headers_mut()
        .insert("x-aokie-device-id", HeaderValue::from_str(subject).unwrap());
    connect_async(request)
        .await
        .expect("authenticated WebSocket")
        .0
}

async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .expect("send v2 frame");
}

async fn receive(socket: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match socket
                .next()
                .await
                .expect("open WebSocket")
                .expect("v2 frame")
            {
                Message::Text(text) => {
                    return serde_json::from_str(&text).expect("valid v2 JSON");
                }
                Message::Ping(payload) => {
                    socket.send(Message::Pong(payload)).await.expect("pong");
                }
                Message::Pong(_) => {}
                other => panic!("unexpected gateway frame: {other:?}"),
            }
        }
    })
    .await
    .expect("gateway response timeout")
}

async fn receive_kind(socket: &mut Socket, expected: &str) -> Value {
    loop {
        let value = receive(socket).await;
        if value["kind"] == expected {
            return value;
        }
    }
}

fn sign_hello_proof(
    challenge: EndpointChallengeFrame,
    signing_key: &SigningKey,
    session_nonce: &str,
) -> SignedHelloProof {
    let now = unix_now();
    challenge.validate(now).expect("fresh endpoint challenge");
    let claims = HelloProofClaims {
        app_id: challenge.app_id,
        subject_id: challenge.subject_id,
        role: challenge.role,
        connection_id: challenge.connection_id,
        challenge_nonce: challenge.challenge_nonce,
        admission_jti: challenge.admission_jti,
        session_nonce: session_nonce.into(),
        holder_key_thumbprint: challenge.holder_key_thumbprint,
        expected_peer_key_thumbprint: challenge.expected_peer_key_thumbprint,
        approved_peer_key_thumbprints: challenge.approved_peer_key_thumbprints,
        peer_roster_revision: challenge.peer_roster_revision,
        peer_roster_hash: challenge.peer_roster_hash,
        nonce: format!("hello_nonce_{}", uuid::Uuid::new_v4().simple()),
        jti: format!("hello_jti_{}", uuid::Uuid::new_v4().simple()),
        issued_at: now,
        expires_at: challenge.expires_at.min(now + 20),
    };
    let signature = URL_SAFE_NO_PAD.encode(
        signing_key
            .sign(&claims.signing_bytes().expect("hello signing bytes"))
            .to_bytes(),
    );
    SignedHelloProof {
        endpoint_key: endpoint_key(signing_key),
        claims,
        signature,
    }
}

async fn receive_challenge(socket: &mut Socket) -> EndpointChallengeFrame {
    serde_json::from_value(receive_kind(socket, "endpoint_challenge").await)
        .expect("endpoint challenge shape")
}

async fn register_endpoints(gateway: &TestGateway) -> (Socket, Socket, Value) {
    let mut plugin = connect(gateway, PLUGIN_ID, &gateway.plugin_token).await;
    let plugin_challenge = receive_challenge(&mut plugin).await;
    send(
        &mut plugin,
        json!({
            "kind":"plugin_hello", "schemaVersion":2, "appId":APP_ID,
            "pluginId":PLUGIN_ID, "sessionNonce":"plugin_nonce_e2e",
            "endpointProof":sign_hello_proof(
                plugin_challenge,
                &plugin_signing_key(),
                "plugin_nonce_e2e"
            )
        }),
    )
    .await;
    send(
        &mut plugin,
        json!({
            "kind":"plugin_snapshot", "schemaVersion":2, "appId":APP_ID,
            "eventId":"snapshot_e2e_1",
            "snapshot":{
                "callId":CALL_ID, "callEpoch":CALL_EPOCH, "ownerEpoch":0,
                "switchboardRevision":10, "remoteRevision":20,
                "telephonyState":"active", "serviceMode":"aokie_active",
                "mediaState":"ready",
                "remoteCapabilities":{
                    "softwareHold":true,
                    "carrierHoldEvidence":"unknown",
                    "secondaryCallObservation":"unknown",
                    "voiceConsult":true,
                    "takeover":true
                },
                "secondaryCallPolicy":"normal",
                "caller":{"label":"Release harness", "maskedNumber":"***123"},
                "remoteConsent":{
                    "policyId":"aokie_remote_access", "policyVersion":3,
                    "enabled":true, "acknowledged":true,
                    "acknowledgedAt":"2026-07-16T00:00:00Z",
                    "expiresAt":"2999-01-01T00:00:00Z",
                    "captionsEnabled":true, "assistanceEnabled":true,
                    "monitorEnabled":true, "consultEnabled":true, "takeoverEnabled":true
                },
                "captions":[{
                    "captionId":"caption_e2e", "speaker":"caller", "text":"hello",
                    "occurredAt":"2026-07-16T00:00:00Z", "finalText":true
                }],
                "occurredAt":"2026-07-16T00:00:00Z"
            }
        }),
    )
    .await;

    let mut mobile = connect(gateway, DEVICE_ID, &gateway.mobile_token).await;
    let mobile_challenge = receive_challenge(&mut mobile).await;
    send(
        &mut mobile,
        json!({
            "kind":"mobile_hello", "schemaVersion":2, "appId":APP_ID,
            "deviceId":DEVICE_ID, "sessionNonce":"mobile_nonce_e2e",
            "endpointProof":sign_hello_proof(
                mobile_challenge,
                &mobile_signing_key(),
                "mobile_nonce_e2e"
            )
        }),
    )
    .await;
    let snapshot = receive_kind(&mut mobile, "snapshot").await;
    assert_eq!(snapshot["snapshot"]["callId"], CALL_ID);
    assert_eq!(snapshot["snapshot"]["ownerEpoch"], 0);
    assert_eq!(snapshot["snapshot"]["caller"]["maskedNumber"], "***123");
    assert_eq!(snapshot["snapshot"]["captions"][0]["text"], "hello");
    assert_eq!(
        snapshot["snapshot"]["pendingMobileOffers"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    (plugin, mobile, snapshot)
}

async fn request_lease(
    mobile: &mut Socket,
    offers_snapshot: &Value,
    request_id: &str,
    mode: &str,
    rtc_session_id: &str,
) {
    let signed_offer = offers_snapshot["snapshot"]["pendingMobileOffers"]
        .as_array()
        .expect("pending mobile offers")
        .iter()
        .find(|signed| {
            signed["offer"]["offeredMode"] == mode && signed["offer"]["surface"] == "in_app"
        })
        .unwrap_or_else(|| panic!("pending {mode} offer"));
    let offer = &signed_offer["offer"];
    send(
        mobile,
        json!({
            "kind":"mobile_offer_answer", "schemaVersion":2, "appId":APP_ID,
            "requestId":format!("answer_{request_id}"),
            "idempotencyKey":format!("answer_idempotency_{request_id}"),
            "offerId":offer["offerId"], "offerJti":offer["jti"],
            "offerToken":signed_offer["offerToken"],
            "targetDeviceId":offer["targetDeviceId"],
            "targetHolderKeyThumbprint":offer["targetHolderKeyThumbprint"],
            "offeredMode":offer["offeredMode"], "callId":offer["callId"],
            "callEpoch":offer["callEpoch"], "ownerEpoch":offer["ownerEpoch"]
        }),
    )
    .await;
    let accepted = receive_kind(mobile, "mobile_offer_accepted").await;
    assert_eq!(accepted["offerId"], offer["offerId"]);
    assert_eq!(accepted["offerJti"], offer["jti"]);
    send(
        mobile,
        json!({
            "kind":"lease_request", "schemaVersion":2, "appId":APP_ID,
            "requestId":request_id, "idempotencyKey":format!("idempotency_{request_id}"),
            "callId":CALL_ID, "expectedCallEpoch":CALL_EPOCH, "expectedOwnerEpoch":0,
            "expectedSwitchboardRevision":10, "expectedRemoteRevision":20,
            "mode":mode, "rtcSessionId":rtc_session_id,
            "acceptedOfferId":offer["offerId"], "acceptedOfferJti":offer["jti"]
        }),
    )
    .await;
}

fn lease(value: &Value) -> LeaseClaims {
    serde_json::from_value(value["lease"].clone()).expect("lease claims")
}

async fn send_mobile_offer(
    mobile: &mut Socket,
    token: &str,
    claims: &LeaseClaims,
    signal_id: &str,
    sdp_revision: u64,
    generation: u64,
    sdp: &str,
) {
    let digest = std::iter::repeat("AA")
        .take(32)
        .collect::<Vec<_>>()
        .join(":");
    let sdp = format!("{sdp}a=fingerprint:sha-256 {digest}\r\n");
    let now = unix_now();
    let signing_key = mobile_signing_key();
    let mobile_key = endpoint_key(&signing_key);
    let claims_binding = EndpointBindingClaims {
        app_id: APP_ID.into(),
        plugin_id: PLUGIN_ID.into(),
        device_id: DEVICE_ID.into(),
        rtc_session_id: claims.rtc_session_id.clone(),
        endpoint_session_nonce: "mobile_nonce_e2e".into(),
        lease_jti: claims.jti.clone(),
        endpoint_role: AdmissionRole::Mobile,
        holder_key_thumbprint: mobile_key.thumbprint.clone(),
        peer_key_thumbprint: endpoint_key(&plugin_signing_key()).thumbprint,
        call_id: claims.call_id.clone(),
        call_epoch: claims.call_epoch,
        owner_epoch: claims.owner_epoch,
        fence: claims.fence,
        sdp_revision,
        transport_generation: generation,
        dtls_fingerprint: sdp_dtls_fingerprint(&sdp).expect("SDP DTLS fingerprint"),
        sdp_sha256: sdp_sha256(&sdp),
        nonce: format!("binding_nonce_{}", uuid::Uuid::new_v4().simple()),
        jti: format!("binding_jti_{}", uuid::Uuid::new_v4().simple()),
        issued_at: now,
        expires_at: now + 20,
    };
    let binding = SignedEndpointBinding {
        signature: URL_SAFE_NO_PAD.encode(
            signing_key
                .sign(
                    &claims_binding
                        .signing_bytes()
                        .expect("endpoint binding signing bytes"),
                )
                .to_bytes(),
        ),
        endpoint_key: mobile_key,
        claims: claims_binding,
    };
    send(
        mobile,
        json!({
            "kind":"rtc_signal", "schemaVersion":2, "appId":APP_ID,
            "signalId":signal_id, "pluginId":PLUGIN_ID, "deviceId":DEVICE_ID,
            "leaseToken":token, "leaseJti":claims.jti,
            "rtcSessionId":claims.rtc_session_id, "sdpRevision":sdp_revision,
            "transportGeneration":generation, "callId":claims.call_id,
            "callEpoch":claims.call_epoch, "ownerEpoch":claims.owner_epoch,
            "fence":claims.fence,
            "signal":{"type":"offer", "sdp":sdp, "binding":binding}
        }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_rotates_two_phase_takeover_and_rejects_every_stale_authority() {
    let gateway = start_gateway().await;
    let (mut plugin, mut mobile, offers_snapshot) = register_endpoints(&gateway).await;

    request_lease(
        &mut mobile,
        &offers_snapshot,
        "monitor_request",
        "monitor",
        "rtc_monitor_e2e",
    )
    .await;
    let monitor_mobile = receive_kind(&mut mobile, "lease_granted").await;
    let monitor_plugin = receive_kind(&mut plugin, "lease_granted").await;
    let monitor = lease(&monitor_mobile);
    assert_eq!(lease(&monitor_plugin), monitor);
    assert_eq!(monitor.mode, LeaseMode::Monitor);
    assert_eq!(monitor.phase, LeasePhase::Active);
    assert_eq!(monitor.fence, 0);
    assert_eq!(
        monitor.tracks,
        vec![MediaTrack::PstnIn, MediaTrack::PstnOut]
    );
    let monitor_binding = binding(&monitor);
    assert_eq!(monitor_binding.mode, MediaMode::Monitor);
    assert!(RoutePermit::new(monitor_binding, Duration::from_secs(1)).is_err());

    request_lease(
        &mut mobile,
        &offers_snapshot,
        "takeover_request",
        "takeover",
        "rtc_takeover_e2e",
    )
    .await;
    let provisional_mobile = receive_kind(&mut mobile, "claim_provisional").await;
    let proposal_plugin = receive_kind(&mut plugin, "claim_proposal").await;
    let provisional = lease(&provisional_mobile);
    assert_eq!(lease(&proposal_plugin), provisional);
    assert_eq!(provisional.phase, LeasePhase::Prepared);
    assert!(provisional.fence > 0);
    assert_eq!(provisional.tracks, vec![MediaTrack::PstnIn]);
    let prepared_binding = binding(&provisional);
    assert_eq!(prepared_binding.mode, MediaMode::PreparedTalk);
    assert!(!prepared_binding.mode.needs_microphone());
    assert!(RoutePermit::new(prepared_binding, Duration::from_secs(1)).is_err());

    let provisional_token = provisional_mobile["leaseToken"]
        .as_str()
        .expect("provisional token")
        .to_owned();
    send_mobile_offer(
        &mut mobile,
        &provisional_token,
        &provisional,
        "prepared_offer_e2e",
        1,
        1,
        "v=0\r\no=prepared 1 1 IN IP4 127.0.0.1\r\n",
    )
    .await;
    let routed_prepared = receive_kind(&mut plugin, "rtc_signal").await;
    assert_eq!(routed_prepared["leaseJti"], provisional.jti);
    assert_eq!(routed_prepared["sdpRevision"], 1);
    assert_eq!(routed_prepared["transportGeneration"], 1);

    // This is the plugin's physical soft-hold/flush acknowledgement.  The
    // gateway refuses an accepted decision unless physical ownerEpoch moves.
    send(
        &mut plugin,
        json!({
            "kind":"claim_decision", "schemaVersion":2, "appId":APP_ID,
            "requestId":"takeover_request", "deviceId":DEVICE_ID,
            "callId":CALL_ID, "callEpoch":CALL_EPOCH, "fence":provisional.fence,
            "accepted":true, "mediaReady":true, "confirmedOwnerEpoch":1,
            "switchboardRevision":11, "remoteRevision":21
        }),
    )
    .await;
    let active_mobile = receive_kind(&mut mobile, "claim_active").await;
    let active = lease(&active_mobile);
    assert_eq!(active.lease_id, provisional.lease_id);
    assert_eq!(active.rtc_session_id, provisional.rtc_session_id);
    assert_ne!(active.jti, provisional.jti);
    assert_eq!(active.owner_epoch, 1);
    assert_eq!(active.phase, LeasePhase::Active);
    assert!(active.tracks.contains(&MediaTrack::PstnOut));
    let active_token = active_mobile["leaseToken"].as_str().unwrap().to_owned();

    // A signed but superseded prepared token is no longer authority.
    send_mobile_offer(
        &mut mobile,
        &provisional_token,
        &provisional,
        "stale_prepared_offer_e2e",
        2,
        2,
        "v=0\r\no=stale 2 2 IN IP4 127.0.0.1\r\n",
    )
    .await;
    let stale = receive_kind(&mut mobile, "error").await;
    assert_eq!(stale["code"], "invalid_lease");

    send_mobile_offer(
        &mut mobile,
        &active_token,
        &active,
        "active_offer_e2e",
        2,
        2,
        "v=0\r\no=active 2 2 IN IP4 127.0.0.1\r\n",
    )
    .await;
    let routed_active = receive_kind(&mut plugin, "rtc_signal").await;
    assert_eq!(routed_active["leaseJti"], active.jti);
    assert_eq!(routed_active["ownerEpoch"], 1);
    assert_eq!(routed_active["sdpRevision"], 2);
    assert_eq!(routed_active["transportGeneration"], 2);
    assert_ne!(
        routed_prepared["signal"]["sdp"], routed_active["signal"]["sdp"],
        "active ownership requires a fresh offer"
    );

    send(
        &mut mobile,
        json!({
            "kind":"lease_revoke", "schemaVersion":2, "appId":APP_ID,
            "requestId":"revoke_active_e2e", "idempotencyKey":"revoke_active_key_e2e",
            "leaseToken":active_token, "reason":"operator_return"
        }),
    )
    .await;
    let revoked_mobile = receive_kind(&mut mobile, "lease_revoked").await;
    let revoked_plugin = receive_kind(&mut plugin, "lease_revoked").await;
    assert_eq!(revoked_mobile["leaseJti"], active.jti);
    assert_eq!(revoked_plugin["leaseJti"], active.jti);

    send_mobile_offer(
        &mut mobile,
        active_mobile["leaseToken"].as_str().unwrap(),
        &active,
        "revoked_active_offer_e2e",
        3,
        3,
        "v=0\r\no=revoked 3 3 IN IP4 127.0.0.1\r\n",
    )
    .await;
    let revoked = receive_kind(&mut mobile, "error").await;
    assert_eq!(revoked["code"], "invalid_lease");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_consult_is_assistance_bound_rotated_and_never_caller_authority() {
    let gateway = start_gateway().await;
    let (mut plugin, mut mobile, offers_snapshot) = register_endpoints(&gateway).await;

    send(
        &mut plugin,
        json!({
            "kind":"assistance_request", "schemaVersion":2, "appId":APP_ID,
            "eventId":"assist_event_e2e", "requestId":"assist_request_e2e",
            "callId":CALL_ID, "callEpoch":CALL_EPOCH, "ownerEpoch":0,
            "switchboardRevision":10, "remoteRevision":20,
            "question":"Which appointment should I offer?",
            "expiresAt":unix_now() + 120
        }),
    )
    .await;
    let assistance = receive_kind(&mut mobile, "assistance_request").await;
    assert_eq!(assistance["requestId"], "assist_request_e2e");

    request_lease(
        &mut mobile,
        &offers_snapshot,
        "consult_request",
        "consult",
        "rtc_consult_e2e",
    )
    .await;
    let provisional_mobile = receive_kind(&mut mobile, "claim_provisional").await;
    let proposal_plugin = receive_kind(&mut plugin, "claim_proposal").await;
    let provisional = lease(&provisional_mobile);
    assert_eq!(lease(&proposal_plugin), provisional);
    assert_eq!(provisional.mode, LeaseMode::Consult);
    assert_eq!(provisional.phase, LeasePhase::Prepared);
    assert_eq!(provisional.fence, 0);
    assert_eq!(provisional.tracks, vec![MediaTrack::ConsultRx]);
    let prepared_binding = binding(&provisional);
    assert_eq!(prepared_binding.mode, MediaMode::PreparedConsult);
    assert!(!prepared_binding.mode.needs_microphone());
    assert!(RoutePermit::new(prepared_binding, Duration::from_secs(1)).is_err());

    let provisional_token = provisional_mobile["leaseToken"]
        .as_str()
        .expect("provisional consult token")
        .to_owned();
    send_mobile_offer(
        &mut mobile,
        &provisional_token,
        &provisional,
        "prepared_consult_offer_e2e",
        1,
        1,
        "v=0\r\no=prepared-consult 1 1 IN IP4 127.0.0.1\r\n",
    )
    .await;
    let routed_prepared = receive_kind(&mut plugin, "rtc_signal").await;
    assert_eq!(routed_prepared["leaseJti"], provisional.jti);

    send(
        &mut plugin,
        json!({
            "kind":"claim_decision", "schemaVersion":2, "appId":APP_ID,
            "requestId":"consult_request", "deviceId":DEVICE_ID,
            "callId":CALL_ID, "callEpoch":CALL_EPOCH, "fence":0,
            "accepted":true, "mediaReady":true, "confirmedOwnerEpoch":1,
            "switchboardRevision":11, "remoteRevision":21
        }),
    )
    .await;
    let active_mobile = receive_kind(&mut mobile, "claim_active").await;
    let active = lease(&active_mobile);
    assert_eq!(active.mode, LeaseMode::Consult);
    assert_eq!(active.phase, LeasePhase::Active);
    assert_eq!(active.fence, 0);
    assert_eq!(active.owner_epoch, 1);
    assert_eq!(active.lease_id, provisional.lease_id);
    assert_eq!(active.rtc_session_id, provisional.rtc_session_id);
    assert_ne!(active.jti, provisional.jti);
    assert_eq!(
        active.tracks,
        vec![MediaTrack::ConsultRx, MediaTrack::ConsultTx]
    );
    let active_binding = binding(&active);
    assert_eq!(active_binding.mode, MediaMode::Consult);
    assert!(active_binding.mode.needs_microphone());
    assert!(
        RoutePermit::new(active_binding, Duration::from_secs(1)).is_err(),
        "consult can never become caller transmit authority"
    );

    send_mobile_offer(
        &mut mobile,
        &provisional_token,
        &provisional,
        "stale_prepared_consult_offer_e2e",
        2,
        2,
        "v=0\r\no=stale-consult 2 2 IN IP4 127.0.0.1\r\n",
    )
    .await;
    let stale = receive_kind(&mut mobile, "error").await;
    assert_eq!(stale["code"], "invalid_lease");

    let active_token = active_mobile["leaseToken"].as_str().unwrap();
    send_mobile_offer(
        &mut mobile,
        active_token,
        &active,
        "active_consult_offer_e2e",
        2,
        2,
        "v=0\r\no=active-consult 2 2 IN IP4 127.0.0.1\r\n",
    )
    .await;
    let routed_active = receive_kind(&mut plugin, "rtc_signal").await;
    assert_eq!(routed_active["leaseJti"], active.jti);
    assert_eq!(routed_active["ownerEpoch"], 1);

    // The Desktop/private-consult worker ends the bounded interaction.  The
    // gateway revokes only this isolated lease; no talk lease is promoted.
    send(
        &mut plugin,
        json!({
            "kind":"plugin_lease_revoke", "schemaVersion":2, "appId":APP_ID,
            "deviceId":DEVICE_ID, "leaseId":active.lease_id,
            "leaseJti":active.jti, "callId":CALL_ID,
            "callEpoch":CALL_EPOCH, "fence":0, "reason":"consult_complete"
        }),
    )
    .await;
    let revoked = receive_kind(&mut mobile, "lease_revoked").await;
    assert_eq!(revoked["leaseJti"], active.jti);
    assert_eq!(revoked["reason"], "consult_complete");
}

fn binding(claims: &LeaseClaims) -> SessionBinding {
    let mode = match (claims.mode, claims.phase) {
        (LeaseMode::Monitor, _) => MediaMode::Monitor,
        (LeaseMode::Consult, LeasePhase::Prepared) => MediaMode::PreparedConsult,
        (LeaseMode::Consult, LeasePhase::Active) => MediaMode::Consult,
        (LeaseMode::Takeover, LeasePhase::Prepared) => MediaMode::PreparedTalk,
        (LeaseMode::Takeover, LeasePhase::Active) => MediaMode::Talk,
    };
    SessionBinding {
        rtc_session_id: claims.rtc_session_id.clone(),
        call_id: claims.call_id.clone(),
        call_epoch: claims.call_epoch,
        owner_epoch: claims.owner_epoch,
        device_id: claims.device_id.clone(),
        mode,
        lease_id: Some(claims.lease_id.clone()),
        fence: claims.fence,
    }
}

async fn wait_for_remote_answer(media: &RemoteMediaHandle, companion: &mut CompanionPeer) {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut answer_received = false;
        let mut companion_connected = false;
        let mut desktop_connected = false;
        while !(answer_received && companion_connected && desktop_connected) {
            for event in media.drain_events(32) {
                match event.kind {
                    RemoteMediaEventKind::SdpAnswer { answer } => {
                        companion.accept_answer(answer).await.expect("native answer");
                        answer_received = true;
                    }
                    RemoteMediaEventKind::LocalIce { candidate } => companion
                        .add_remote_candidate(candidate)
                        .await
                        .expect("Companion remote ICE"),
                    RemoteMediaEventKind::ConnectionState { state } if state == "connected" => {
                        desktop_connected = true;
                    }
                    RemoteMediaEventKind::ProtocolViolation { message } => {
                        panic!("Desktop protocol violation: {message}")
                    }
                    RemoteMediaEventKind::Error { operation, message } => {
                        panic!("Desktop media {operation}: {message}")
                    }
                    _ => {}
                }
            }
            if answer_received {
                tokio::select! {
                    event = companion.next_event() => match event.expect("Companion event lane") {
                        PeerEvent::LocalIce(candidate) => media
                            .add_remote_ice(companion.binding().rtc_session_id.as_str(), candidate)
                            .expect("Desktop remote ICE"),
                        PeerEvent::ConnectionState("connected") => companion_connected = true,
                        PeerEvent::ProtocolViolation(message) => panic!("Companion protocol violation: {message}"),
                        _ => {}
                    },
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    })
    .await
    .expect("native peer connection timeout");
}

async fn open_native_peer(media: &RemoteMediaHandle, binding: SessionBinding) -> CompanionPeer {
    let (mut companion, offer) = CompanionPeer::offer(binding.clone(), PeerOptions::default())
        .await
        .expect("Companion offer");
    media
        .open_peer(OpenPeerRequest {
            binding,
            offer,
            lease_ttl_ms: 15_000,
            ice_servers: vec![],
            relay_only: false,
        })
        .expect("Desktop peer");
    wait_for_remote_answer(media, &mut companion).await;
    companion
}

fn wait_for_talk_pcm(media: &RemoteMediaHandle) -> Option<aokie_media::OwnedAudioFrame> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Some(frame) = media.try_recv_talk_pcm() {
            return Some(frame);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Adds the real libwebrtc + operating-system microphone boundary to the
/// protocol assertions above.  It is deliberately opt-in because a headless
/// machine has no meaningful audio endpoint to verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local operating-system microphone and speaker endpoint"]
async fn native_plugin_router_opens_only_the_exact_physically_acked_active_route() {
    let media = RemoteMediaHandle::spawn().expect("remote media runtime");
    media.set_remote_consent(full_remote_consent());
    media.observe_physical_call(Some(CALL_ID), true);
    assert_eq!(media.snapshot().call_epoch, CALL_EPOCH);

    let monitor = SessionBinding {
        rtc_session_id: "rtc_native_monitor_e2e".into(),
        call_id: CALL_ID.into(),
        call_epoch: CALL_EPOCH,
        owner_epoch: 0,
        device_id: DEVICE_ID.into(),
        mode: MediaMode::Monitor,
        lease_id: Some("lease_native_monitor_e2e".into()),
        fence: 0,
    };
    let monitor_peer = open_native_peer(&media, monitor.clone()).await;
    assert!(!monitor_peer.microphone_active());
    assert!(monitor_peer
        .arm_microphone(&monitor, Duration::from_secs(1))
        .is_err());
    assert!(RoutePermit::new(monitor.clone(), Duration::from_secs(1)).is_err());
    media.try_push_sco(&[0; 160], 16_000);
    assert!(media.try_recv_talk_pcm().is_none());
    monitor_peer.close();
    media
        .close_peer(&monitor.rtc_session_id, "monitor_complete")
        .expect("close monitor peer");

    let prepared = SessionBinding {
        rtc_session_id: "rtc_native_takeover_e2e".into(),
        call_id: CALL_ID.into(),
        call_epoch: CALL_EPOCH,
        owner_epoch: 0,
        device_id: DEVICE_ID.into(),
        mode: MediaMode::PreparedTalk,
        lease_id: Some("lease_native_takeover_e2e".into()),
        fence: 1,
    };
    let prepared_peer = open_native_peer(&media, prepared.clone()).await;
    assert!(!prepared_peer.microphone_active());
    assert!(prepared_peer
        .arm_microphone(&prepared, Duration::from_secs(1))
        .is_err());
    assert!(RoutePermit::new(prepared.clone(), Duration::from_secs(1)).is_err());
    media
        .request_soft_hold(prepared.clone(), 10_000)
        .expect("soft hold request");
    assert_eq!(
        media.next_radio_transition(),
        Some(RadioTransition::PrepareHuman {
            binding: prepared.clone()
        })
    );
    media
        .ack_prepare_human(&prepared)
        .expect("physical prepared ACK");
    assert_eq!(media.snapshot().owner_epoch, 1);
    assert_eq!(media.snapshot().service_mode, ServiceMode::HumanPending);
    assert!(media.try_recv_talk_pcm().is_none());

    // `claim_active` rotates the JTI/owner epoch.  Native media must close the
    // receive-only provisional peer and negotiate a fresh sendrecv peer.
    prepared_peer.close();
    media
        .close_peer(&prepared.rtc_session_id, "active_rebind")
        .expect("close prepared peer");
    let active = SessionBinding {
        owner_epoch: 1,
        mode: MediaMode::Talk,
        ..prepared.clone()
    };
    let active_peer = open_native_peer(&media, active.clone()).await;
    assert!(!active_peer.microphone_active());
    assert!(active_peer
        .arm_microphone(&prepared, Duration::from_secs(2))
        .is_err());
    media
        .request_takeover(active.clone(), 10_000)
        .expect("active takeover request");
    assert!(media.try_recv_talk_pcm().is_none());
    assert_eq!(
        media.next_radio_transition(),
        Some(RadioTransition::EnterHuman {
            binding: active.clone()
        })
    );
    media
        .ack_enter_human(&active)
        .expect("physical enter-human ACK");
    active_peer
        .arm_microphone(&active, Duration::from_secs(5))
        .expect("exact active microphone lease");
    let pcm = tokio::task::spawn_blocking({
        let media = media.clone();
        move || wait_for_talk_pcm(&media)
    })
    .await
    .expect("PCM join")
    .expect("authorized operating-system microphone PCM");
    assert!(pcm.is_ten_milliseconds());
    assert_eq!(media.snapshot().service_mode, ServiceMode::HumanActive);

    // Force a short final lease and let it expire.  The Desktop gate revokes
    // itself before the physical return ACK; any already-buffered microphone
    // frames are revalidated and quarantined by the outer router.
    media
        .renew_lease(active.clone(), 50)
        .expect("short active lease");
    tokio::time::sleep(Duration::from_millis(80)).await;
    active_peer.disarm_microphone();
    assert!(!active_peer.microphone_active());
    assert_eq!(
        media.next_radio_transition(),
        Some(RadioTransition::ReturnToAokie {
            reason: "lease_expired".into()
        })
    );
    assert_eq!(media.snapshot().peer_count, 0, "expiry closes DesktopPeer");
    assert!(media.try_recv_talk_pcm().is_none());
    media.ack_return_to_aokie().expect("physical return ACK");
    assert_eq!(media.snapshot().service_mode, ServiceMode::AokieActive);
    assert!(!media.radio_reserved());
    assert!(media.try_recv_talk_pcm().is_none());

    // Even if stale code retained the old native object, neither a stale
    // binding nor a closed/revoked route can reopen caller transmit.
    assert!(active_peer
        .arm_microphone(&prepared, Duration::from_secs(1))
        .is_err());
    active_peer.close();
}
