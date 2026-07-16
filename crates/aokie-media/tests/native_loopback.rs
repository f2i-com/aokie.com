use std::time::Duration;

use aokie_media::{
    CompanionPeer, DesktopPeer, IceServerConfig, MediaMode, PeerEvent, PeerOptions, RoutePermit,
    SessionBinding,
};

fn monitor_binding() -> SessionBinding {
    SessionBinding {
        rtc_session_id: "rtc_local_monitor".into(),
        call_id: "call_local".into(),
        call_epoch: 1,
        owner_epoch: 1,
        device_id: "device_local".into(),
        mode: MediaMode::Monitor,
        lease_id: Some("lease_local_monitor".into()),
        fence: 0,
    }
}

fn talk_binding() -> SessionBinding {
    SessionBinding {
        rtc_session_id: "rtc_local_talk".into(),
        call_id: "call_local".into(),
        call_epoch: 2,
        owner_epoch: 3,
        device_id: "device_local".into(),
        mode: MediaMode::Talk,
        lease_id: Some("lease_local".into()),
        fence: 7,
    }
}

fn prepared_talk_binding() -> SessionBinding {
    SessionBinding {
        rtc_session_id: "rtc_local_prepared".into(),
        call_id: "call_local".into(),
        call_epoch: 2,
        owner_epoch: 2,
        device_id: "device_local".into(),
        mode: MediaMode::PreparedTalk,
        lease_id: Some("lease_local_prepared".into()),
        fence: 7,
    }
}

async fn connect(
    companion: &mut CompanionPeer,
    desktop: &mut DesktopPeer,
) -> Result<(), &'static str> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut companion_connected = false;
        let mut desktop_connected = false;
        while !companion_connected || !desktop_connected {
            tokio::select! {
                event = companion.next_event() => match event.expect("Companion event lane") {
                    PeerEvent::LocalIce(candidate) => desktop.add_remote_candidate(candidate).await.expect("Desktop ICE"),
                    PeerEvent::ConnectionState("connected") => companion_connected = true,
                    PeerEvent::ProtocolViolation(reason) => panic!("Companion protocol violation: {reason}"),
                    _ => {}
                },
                event = desktop.next_event() => match event.expect("Desktop event lane") {
                    PeerEvent::LocalIce(candidate) => companion.add_remote_candidate(candidate).await.expect("Companion ICE"),
                    PeerEvent::ConnectionState("connected") => desktop_connected = true,
                    PeerEvent::ProtocolViolation(reason) => panic!("Desktop protocol violation: {reason}"),
                    _ => {}
                },
            }
        }
    })
    .await
    .map_err(|_| "native peers did not connect before timeout")
}

async fn connect_over_forced_relay(
    companion: &mut CompanionPeer,
    desktop: &mut DesktopPeer,
) -> Result<(usize, usize), &'static str> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut companion_connected = false;
        let mut desktop_connected = false;
        let mut companion_relay_candidates = 0;
        let mut desktop_relay_candidates = 0;
        while !companion_connected || !desktop_connected {
            tokio::select! {
                event = companion.next_event() => match event.expect("Companion event lane") {
                    PeerEvent::LocalIce(candidate) => {
                        eprintln!("Companion forced-relay candidate: {}", candidate.candidate);
                        assert!(is_relay_candidate(&candidate.candidate), "relay-only Companion emitted a non-relay candidate: {}", candidate.candidate);
                        companion_relay_candidates += 1;
                        desktop.add_remote_candidate(candidate).await.expect("Desktop relay ICE");
                    }
                    PeerEvent::ConnectionState("connected") => companion_connected = true,
                    PeerEvent::ConnectionState("failed") => panic!("Companion forced-relay ICE failed"),
                    PeerEvent::ProtocolViolation(reason) => panic!("Companion protocol violation: {reason}"),
                    _ => {}
                },
                event = desktop.next_event() => match event.expect("Desktop event lane") {
                    PeerEvent::LocalIce(candidate) => {
                        eprintln!("Desktop forced-relay candidate: {}", candidate.candidate);
                        assert!(is_relay_candidate(&candidate.candidate), "relay-only Desktop emitted a non-relay candidate: {}", candidate.candidate);
                        desktop_relay_candidates += 1;
                        companion.add_remote_candidate(candidate).await.expect("Companion relay ICE");
                    }
                    PeerEvent::ConnectionState("connected") => desktop_connected = true,
                    PeerEvent::ConnectionState("failed") => panic!("Desktop forced-relay ICE failed"),
                    PeerEvent::ProtocolViolation(reason) => panic!("Desktop protocol violation: {reason}"),
                    _ => {}
                },
            }
        }
        assert!(companion_relay_candidates > 0, "Companion emitted no relay candidate");
        assert!(desktop_relay_candidates > 0, "Desktop emitted no relay candidate");
        (companion_relay_candidates, desktop_relay_candidates)
    })
    .await
    .map_err(|_| "native peers did not connect over TURN before timeout")
}

fn is_relay_candidate(candidate: &str) -> bool {
    let fields = candidate.split_ascii_whitespace().collect::<Vec<_>>();
    fields
        .windows(2)
        .any(|fields| fields[0] == "typ" && fields[1] == "relay")
}

fn forced_relay_options() -> PeerOptions {
    PeerOptions {
        ice_servers: vec![IceServerConfig {
            urls: vec![std::env::var("AOKIE_TEST_TURN_URL")
                .unwrap_or_else(|_| "turn:127.0.0.1:34780?transport=tcp".into())],
            username: std::env::var("AOKIE_TEST_TURN_USERNAME")
                .unwrap_or_else(|_| "aokie-test".into()),
            credential: std::env::var("AOKIE_TEST_TURN_CREDENTIAL")
                .unwrap_or_else(|_| "aokie-test-password".into()),
        }],
        relay_only: true,
        ..PeerOptions::default()
    }
}

/// Runs both native endpoints through coturn with `IceTransportsType::Relay`.
/// The companion remains receive-only, so this validates TURN allocation,
/// relay-only candidate filtering, ICE connectivity and caller-audio delivery
/// without opening microphone/caller-transmit authority.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the local Docker coturn forced-relay harness and an operating-system speaker endpoint"]
async fn native_monitor_peer_connects_over_forced_turn_relay() {
    let binding = SessionBinding {
        rtc_session_id: "rtc_local_turn_monitor".into(),
        lease_id: Some("lease_local_turn_monitor".into()),
        ..monitor_binding()
    };
    let options = forced_relay_options();
    let (mut companion, offer) = CompanionPeer::offer(binding.clone(), options.clone())
        .await
        .expect("Companion forced-relay monitor offer");
    let (mut desktop, answer) = DesktopPeer::answer(binding, offer, options)
        .await
        .expect("Desktop forced-relay answer");
    companion
        .accept_answer(answer)
        .await
        .expect("Companion accepts forced-relay answer");

    let (companion_candidates, desktop_candidates) =
        connect_over_forced_relay(&mut companion, &mut desktop)
            .await
            .unwrap();
    assert!(companion_candidates > 0 && desktop_candidates > 0);
    assert!(!companion.microphone_active());
    assert_eq!(desktop.push_caller_pcm(&vec![0; 160]).await.unwrap(), 1);
    assert!(desktop.recv_caller_microphone().await.is_err());

    companion.close();
    desktop.close();
}

/// Exercises the actual native libwebrtc peer connection and the operating
/// system speaker module. It is ignored in routine automation because a
/// headless runner may have no audio endpoint; release verification runs it
/// explicitly on the target Desktop/Companion machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local operating-system audio endpoint"]
async fn native_monitor_peer_connects_without_opening_a_microphone() {
    let binding = monitor_binding();
    let (mut companion, offer) = CompanionPeer::offer(binding.clone(), PeerOptions::default())
        .await
        .expect("Companion monitor offer");
    assert!(!companion.microphone_active());

    let (mut desktop, answer) = DesktopPeer::answer(binding, offer, PeerOptions::default())
        .await
        .expect("Desktop answer");
    companion
        .accept_answer(answer)
        .await
        .expect("Companion accepts answer");

    connect(&mut companion, &mut desktop).await.unwrap();
    assert!(!companion.microphone_active());

    // A real SCO-sized 10 ms frame reaches libwebrtc's Desktop source. The
    // monitor peer has no outbound track in the opposite direction.
    assert_eq!(desktop.push_caller_pcm(&vec![0; 160]).await.unwrap(), 1);
    assert!(desktop.recv_caller_microphone().await.is_err());
    assert!(!companion.microphone_active());

    companion.close();
    desktop.close();
}

/// A provisional takeover has already won a positive fence but has not yet
/// advanced physical ownership. It must be able to receive caller/hold audio
/// while remaining structurally incapable of publishing microphone audio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local operating-system audio endpoint"]
async fn native_prepared_takeover_is_receive_only_despite_positive_fence() {
    let binding = prepared_talk_binding();
    let (mut companion, offer) = CompanionPeer::offer(binding.clone(), PeerOptions::default())
        .await
        .expect("Companion prepared takeover offer");
    let (mut desktop, answer) = DesktopPeer::answer(binding.clone(), offer, PeerOptions::default())
        .await
        .expect("Desktop prepared takeover answer");
    companion
        .accept_answer(answer)
        .await
        .expect("Companion accepts prepared answer");
    connect(&mut companion, &mut desktop).await.unwrap();

    assert!(!companion.microphone_active());
    assert!(companion
        .arm_microphone(&binding, Duration::from_secs(1))
        .is_err());
    assert!(RoutePermit::new(binding.clone(), Duration::from_secs(1)).is_err());
    assert!(desktop.recv_caller_microphone().await.is_err());
    assert_eq!(desktop.push_caller_pcm(&vec![0; 160]).await.unwrap(), 1);

    companion.close();
    desktop.close();
}

/// Verifies the real operating-system microphone path and, critically, that
/// decoded PCM is unavailable until both the native endpoint and Desktop's
/// independent caller-route gate hold the exact current lease/fence binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local microphone and speaker endpoint"]
async fn native_talk_peer_releases_microphone_pcm_only_under_current_lease() {
    let binding = talk_binding();
    let (mut companion, offer) = CompanionPeer::offer(binding.clone(), PeerOptions::default())
        .await
        .expect("Companion talk offer");
    let (mut desktop, answer) = DesktopPeer::answer(binding.clone(), offer, PeerOptions::default())
        .await
        .expect("Desktop answer");
    companion
        .accept_answer(answer)
        .await
        .expect("Companion accepts answer");
    connect(&mut companion, &mut desktop).await.unwrap();

    assert!(!companion.microphone_active());
    assert!(
        tokio::time::timeout(Duration::from_millis(150), desktop.recv_caller_microphone())
            .await
            .is_err()
    );

    desktop
        .authorize_caller_transmit(
            RoutePermit::new(binding.clone(), Duration::from_secs(3)).unwrap(),
        )
        .unwrap();
    companion
        .arm_microphone(&binding, Duration::from_secs(3))
        .expect("current talk lease opens native microphone");
    let frame = tokio::time::timeout(Duration::from_secs(3), desktop.recv_caller_microphone())
        .await
        .expect("microphone frame timeout")
        .expect("authorized microphone frame");
    assert!(frame.is_ten_milliseconds());
    assert!(companion.microphone_active());

    desktop.revoke_caller_transmit();
    companion.disarm_microphone();
    assert!(!companion.microphone_active());
    companion.close();
    desktop.close();
}
