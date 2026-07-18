use std::time::{Duration, Instant};

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
) -> Result<bool, &'static str> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut companion_connected = false;
        let mut desktop_connected = false;
        let mut companion_remote_audio = false;
        while !companion_connected || !desktop_connected {
            tokio::select! {
                event = companion.next_event() => match event.expect("Companion event lane") {
                    PeerEvent::LocalIce(candidate) => desktop.add_remote_candidate(candidate).await.expect("Desktop ICE"),
                    PeerEvent::ConnectionState("connected") => companion_connected = true,
                    PeerEvent::RemoteAudioReady => companion_remote_audio = true,
                    PeerEvent::ProtocolViolation(reason) => panic!("Companion protocol violation: {reason}"),
                    _ => {}
                },
                event = desktop.next_event() => match event.expect("Desktop event lane") {
                    PeerEvent::LocalIce(candidate) => companion.add_remote_candidate(candidate).await.expect("Companion ICE"),
                    PeerEvent::ConnectionState("connected") => desktop_connected = true,
                    PeerEvent::RemoteMicrophoneReady => panic!(
                        "Desktop observed microphone PCM before Companion arm_microphone"
                    ),
                    PeerEvent::ProtocolViolation(reason) => panic!("Desktop protocol violation: {reason}"),
                    _ => {}
                },
            }
        }
        companion_remote_audio
    })
    .await
    .map_err(|_| "native peers did not connect before timeout")
}

async fn wait_for_desktop_remote_microphone(desktop: &mut DesktopPeer) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match desktop.next_event().await.expect("Desktop event lane") {
                PeerEvent::RemoteMicrophoneReady => return,
                PeerEvent::ProtocolViolation(reason) => {
                    panic!("Desktop protocol violation: {reason}")
                }
                PeerEvent::ConnectionState("failed" | "disconnected" | "closed") => {
                    panic!("Desktop connection failed before microphone PCM proof")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("Desktop did not observe decoded microphone PCM after arm_microphone");
}

async fn wait_for_companion_microphone_authority(companion: &CompanionPeer) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !companion.microphone_authority_ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Companion microphone authority channel did not open");
}

async fn assert_no_desktop_microphone_before_arm(desktop: &mut DesktopPeer) {
    let _ = tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            match desktop.next_event().await.expect("Desktop event lane") {
                PeerEvent::RemoteMicrophoneReady => panic!(
                    "Desktop observed disabled-track or synthetic microphone PCM before arm_microphone"
                ),
                PeerEvent::ProtocolViolation(reason) => {
                    panic!("Desktop protocol violation: {reason}")
                }
                PeerEvent::ConnectionState("failed" | "disconnected" | "closed") => {
                    panic!("Desktop connection failed during the pre-arm quiet window")
                }
                _ => {}
            }
        }
    })
    .await;
}

async fn wait_for_companion_remote_audio(companion: &mut CompanionPeer) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match companion.next_event().await.expect("Companion event lane") {
                PeerEvent::RemoteAudioReady => return,
                PeerEvent::ProtocolViolation(reason) => {
                    panic!("Companion protocol violation: {reason}")
                }
                PeerEvent::ConnectionState("failed" | "disconnected" | "closed") => {
                    panic!("Companion connection failed before remote audio")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("Companion did not observe the negotiated caller audio track");
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
                    PeerEvent::RemoteMicrophoneReady => panic!(
                        "Desktop observed microphone PCM before Companion arm_microphone"
                    ),
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

    let remote_audio_ready = connect(&mut companion, &mut desktop).await.unwrap();
    assert!(!companion.microphone_active());

    // A real SCO-sized 10 ms frame reaches libwebrtc's Desktop source. The
    // monitor peer has no outbound track in the opposite direction.
    assert_eq!(desktop.push_caller_pcm(&vec![0; 160]).await.unwrap(), 1);
    if !remote_audio_ready {
        wait_for_companion_remote_audio(&mut companion).await;
    }
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
    let remote_audio_ready = connect(&mut companion, &mut desktop).await.unwrap();

    assert!(!companion.microphone_active());
    assert!(companion
        .arm_microphone(&binding, Duration::from_secs(1))
        .is_err());
    assert!(RoutePermit::new(binding.clone(), Duration::from_secs(1)).is_err());
    assert!(desktop.recv_caller_microphone().await.is_err());
    assert_eq!(desktop.push_caller_pcm(&vec![0; 160]).await.unwrap(), 1);
    if !remote_audio_ready {
        wait_for_companion_remote_audio(&mut companion).await;
    }

    companion.close();
    desktop.close();
}

/// Closing an active-mode peer before any microphone authority was granted
/// must not leak native data-channel/track handles or emit a false readiness
/// edge during teardown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local microphone and speaker endpoint"]
async fn native_talk_peer_closes_cleanly_before_microphone_arm() {
    let binding = talk_binding();
    let (mut companion, offer) = CompanionPeer::offer(binding.clone(), PeerOptions::default())
        .await
        .expect("Companion talk offer");
    let (mut desktop, answer) = DesktopPeer::answer(binding, offer, PeerOptions::default())
        .await
        .expect("Desktop answer");
    companion
        .accept_answer(answer)
        .await
        .expect("Companion accepts answer");
    let _ = connect(&mut companion, &mut desktop).await.unwrap();
    wait_for_companion_microphone_authority(&companion).await;
    assert_no_desktop_microphone_before_arm(&mut desktop).await;
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
    let _ = connect(&mut companion, &mut desktop).await.unwrap();
    wait_for_companion_microphone_authority(&companion).await;

    assert!(!companion.microphone_active());
    assert_no_desktop_microphone_before_arm(&mut desktop).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), desktop.recv_caller_microphone())
            .await
            .is_err()
    );

    companion
        .arm_microphone(&binding, Duration::from_secs(3))
        .expect("current talk lease opens native microphone");
    // Readiness is proven by a decoded frame on the direct Talk peer while
    // Desktop's route is still closed. The proof frame remains quarantined;
    // only after this event may the caller-bound route be authorized.
    wait_for_desktop_remote_microphone(&mut desktop).await;
    desktop
        .authorize_caller_transmit(
            RoutePermit::new(binding.clone(), Duration::from_secs(3)).unwrap(),
        )
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(3), desktop.recv_caller_microphone())
        .await
        .expect("microphone frame timeout")
        .expect("authorized microphone frame");
    assert!(frame.is_ten_milliseconds());
    assert!(companion.microphone_active());

    // Keep the independently authorized Desktop route open while native
    // capture disarms. The exact disarm marker must close PCM delivery even
    // though libwebrtc continues producing concealed/synthetic playout.
    companion.disarm_microphone();
    assert!(!companion.microphone_active());
    tokio::time::sleep(Duration::from_millis(300)).await;
    while matches!(
        tokio::time::timeout(Duration::from_millis(10), desktop.recv_caller_microphone()).await,
        Ok(Ok(_))
    ) {}
    assert!(
        tokio::time::timeout(Duration::from_millis(200), desktop.recv_caller_microphone())
            .await
            .is_err(),
        "concealed PCM escaped after exact native disarm"
    );
    desktop.revoke_caller_transmit();
    companion.close();
    desktop.close();
}

/// A lease refresh must extend the watchdog in place. Stopping and restarting
/// the ADM for every renewal causes audible transmit gaps and repeatedly drops
/// the Companion back to a non-live state even though the route is unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local microphone and speaker endpoint"]
async fn native_talk_lease_renewal_keeps_microphone_capture_continuous() {
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
    let _ = connect(&mut companion, &mut desktop).await.unwrap();
    wait_for_companion_microphone_authority(&companion).await;

    let armed_at = Instant::now();
    companion
        .arm_microphone(&binding, Duration::from_secs(3))
        .expect("current talk lease opens native microphone");
    wait_for_desktop_remote_microphone(&mut desktop).await;
    desktop
        .authorize_caller_transmit(
            RoutePermit::new(binding.clone(), Duration::from_secs(3)).unwrap(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), desktop.recv_caller_microphone())
        .await
        .expect("initial microphone frame timeout")
        .expect("initial authorized microphone frame");

    companion
        .renew_microphone_lease(&binding, Duration::from_secs(6))
        .expect("renewal extends the active native microphone");
    desktop
        .renew_caller_transmit(Duration::from_secs(6))
        .expect("renewal extends the exact Desktop route");
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        armed_at + Duration::from_millis(3_200),
    ))
    .await;
    assert!(
        companion.microphone_active(),
        "the original watchdog must not stop a renewed microphone"
    );
    tokio::time::timeout(Duration::from_secs(2), desktop.recv_caller_microphone())
        .await
        .expect("renewed microphone frame timeout")
        .expect("renewed authorized microphone frame");

    companion.disarm_microphone();
    companion.close();
    desktop.close();
}
