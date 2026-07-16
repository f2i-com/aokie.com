use crate::aokie_radio::pairing_store::AokiePairingStore;
use crate::aokie_radio::transport::{self as winusb, AokieHciTransport};
use crate::aokie_radio::{hci, hfp, l2cap, sco};
use std::time::{Duration, Instant};

pub const AOKIE_CLASSIC_NAME: &str = "Aokie AI Assistant";
/// CoD revert (2026-04-29): full rollback to the value used while audio
/// last worked end-to-end (efa17f2 era).
///
///   0x200408 = Audio service class (bit 21) + AV major device class
///   + Hands-free Device minor (0x08).
///
/// History — be careful, this is a hot spot:
///   - 0x740420 (commit 91fd360, 2026-04-27): added Telephony / OBEX /
///     Rendering service-class bits AND switched minor to Car Audio
///     (0x20) so Pixel surfaces the MAP/PBAP permission toggle alongside
///     the headset-audio one. MAP toggle did appear; SCO audio stopped
///     flowing. Symptom: HCI SCO link comes up, USB iso URBs complete,
///     every microframe descriptor at len=0/status=0, 0 inbound bytes
///     for the entire call.
///   - 0x740408 (2026-04-29 surgical test): kept the new service-class
///     bits but reverted minor to Hands-free Device. Same RX-empty
///     fingerprint. Ruled out "minor class alone".
///
/// MAP/PBAP discovery on Pixel generally follows EIR UUIDs +
/// SDP-resolvable records, not the CoD bits — so AOKIE_EIR_UUIDS
/// should still surface the MAP toggle even with the audio-friendly
/// CoD.
pub const AOKIE_CLASS_OF_DEVICE: u32 = 0x200408;
/// Service-class UUIDs we publish in the Extended Inquiry Response so
/// Pixel/Android sees what we serve before SDP. Only UUIDs that have
/// a corresponding SDP record (returns >0 matches when queried) — when
/// the EIR claims a UUID but the SDP search returns 0 records, Pixel
/// treats the whole UUID list as suspect and skips registering us with
/// the corresponding profile, which suppresses the per-device toggles.
pub const AOKIE_EIR_UUIDS: &[u16] = &[
    0x111e, // HFP-HF
    0x1133, // MAP MNS
    0x1200, // PnP Information
];
pub const AOKIE_PAGE_TIMEOUT: u16 = 0x6000;
pub const AOKIE_LINK_POLICY: u16 = 0x0005;
pub const AOKIE_VOICE_SETTING: u16 = 0x0060;
/// Voice setting for HFP wide-band speech (mSBC) over an eSCO link.
///
/// Bits, MSB-first per Core spec §7.3.32:
///   - Input coding   = Linear (00)
///   - Input data fmt = 2's complement (10)
///   - Sample size    = 8-bit  (0)
///   - PCM bit pos    = 0 (000)
///   - Air coding     = Transparent (11)
/// → `0b0000 0100 0011 = 0x0043`. We mirror BTstack's
///   `hci_set_sco_voice_setting` value here (`btstack/src/hci.c`).
///
/// Bit 5 (sample size) governs how the controller interprets host→
/// controller SCO bytes. With bit 5 set (0x0063) the controller expects
/// 16-bit aligned samples; feeding it a byte stream of pre-encoded mSBC
/// payload then produces silence on the air because every odd byte gets
/// treated as the low half of a sample. With bit 5 clear the controller
/// forwards bytes verbatim — which is what we want, since mSBC is already
/// fully encoded by the host. RX direction is unaffected by this bit.
/// Pairs with `sco_alt_setting_for_voice` mapping a single transparent
/// connection to `SCO_ALT_SETTINGS_8_BIT[0]` = alt 1 (MPS=9).
pub const AOKIE_VOICE_SETTING_TRANSPARENT: u16 = 0x0043;
/// HFP T2 max-latency in ms — the only eSCO param that differs from the
/// "don't-care" S4 default we use for CVSD. T2 is the wide-band-speech
/// link parameter set defined in HFP §5.7.1.4 (table 5.5).
pub const HFP_T2_MAX_LATENCY_MS: u16 = 13;
/// HFP T2 retransmission effort: optimize for link quality (= 0x02).
pub const HFP_T2_RETRANSMISSION_EFFORT: u8 = 0x02;
pub const AOKIE_SCAN_ENABLE_CONNECTABLE_DISCOVERABLE: u8 = 0x03;
/// At-rest scan-enable (audit AOK-BT-001): page scan ON so a BONDED phone can
/// reconnect, inquiry scan OFF so we are NOT discoverable to strangers. The
/// radio only advertises as discoverable while an explicit pairing window is
/// open — see `PairingWindow` and the reconciliation in the runtime loop.
pub const AOKIE_SCAN_ENABLE_CONNECTABLE: u8 = 0x02;
pub const AOKIE_EVENT_MASK: u64 = 0x3fffffff_ffffffff;
pub const AOKIE_LEGACY_PIN: &str = "0000";
// Roughly 60 s of 8 kHz mono audio. The TTS pipeline can dump a whole
// utterance into the queue in a sub-second burst, so a small queue
// drops most of it before the SCO TX loop drains it. 480k samples ≈
// 1 MB of memory — fine for a single in-flight call.
pub const AOKIE_SCO_TX_QUEUE_SAMPLES: usize = 480_000;
/// HCI SCO payload bytes per outgoing packet — used for both CVSD and
/// mSBC over USB.
///
/// BTStack reference (`btstack/src/hci.c:4877-4885`):
/// ```c
/// if (hci_have_usb_transport()){
///     payload_length = 24;     // hard override, regardless of codec
/// }
/// ```
/// Verified working on Broadcom BCM20702A0 (0a5c:21ec): same dongle on
/// the same Zadig/libwdi binding carries audible bot audio with 24-byte
/// payloads via the BTStack-bridge worktree (commit 05e3d51) but goes
/// silent with our prior 48-byte choice. The earlier 48-byte derivation
/// in `project_msbc_iso_raw_passthrough.md` was off the spec / CSR8510
/// dump; the BTStack reference disagrees and BTStack is the path that
/// actually delivers bytes on this controller family.
///
/// 24-byte payload + 3-byte HCI header = 27-byte HCI SCO packet, which
/// fits in 2 microframes at alt 2 / MPS=17 (2×17=34, 27 < 34).
pub const AOKIE_SCO_USB_PAYLOAD_BYTES: usize = 24;
pub const AOKIE_SCO_WRITE_SILENCE_ENV: &str = "AOKIE_RADIO_SCO_TX_SILENCE";
pub const AOKIE_SCO_WRITE_TONE_ENV: &str = "AOKIE_RADIO_SCO_TX_TONE";
pub const AOKIE_SCO_TX_TONE_SAMPLE_RATE: usize = 8_000;
pub const AOKIE_SCO_TX_TONE_HZ: usize = 440;
pub const AOKIE_SCO_TX_TONE_AMPLITUDE: i16 = 4_000;
pub const AOKIE_HFP_AUTO_ANSWER_ENV: &str = "AOKIE_RADIO_AUTO_ANSWER";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerInitReport {
    pub device_path: String,
    pub local_address: String,
    pub version: hci::LocalVersion,
    pub features: [u8; 8],
    pub buffer_size: hci::BufferSize,
    pub local_name: String,
    pub class_of_device: u32,
    pub page_timeout: u16,
    pub link_policy: u16,
    pub voice_setting: u16,
    pub scan_enable: u8,
    pub simple_pairing_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerEventRecord {
    pub event_code: u8,
    pub name: String,
    pub summary: String,
    pub action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerListenReport {
    pub init: ControllerInitReport,
    pub events: Vec<ControllerEventRecord>,
    pub timed_out: bool,
    pub pairing_store_path: String,
    pub stored_link_keys: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclExchangeRecord {
    pub inbound_len: usize,
    pub responses_sent: usize,
    pub response_lengths: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfpEventRecord {
    pub name: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HfpControlReport {
    pub auto_answer_enabled: bool,
    pub answer_attempts: usize,
    pub answer_packets_sent: usize,
    pub last_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScoAudioReport {
    pub packets: usize,
    pub payload_bytes: usize,
    pub bad_packets: usize,
    pub active_connection_handle: Option<u16>,
    pub last_connection_handle: Option<u16>,
    pub last_packet_status_flag: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScoTransportReport {
    pub alternate_setting: Option<u8>,
    pub in_pipe_id: Option<u8>,
    pub out_pipe_id: Option<u8>,
    pub max_packet_size: Option<u16>,
    pub isoch_buffer_len: Option<usize>,
    pub isoch_buffers_registered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScoPcmReport {
    pub codec: Option<String>,
    pub sample_rate: Option<u16>,
    pub frames: usize,
    pub samples: usize,
    pub bad_frames: usize,
    pub min_sample: Option<i16>,
    pub max_sample: Option<i16>,
    pub last_frame_samples: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScoTxReport {
    pub packets: usize,
    pub payload_bytes: usize,
    pub queued_samples: usize,
    pub dropped_samples: usize,
    pub silence_packets: usize,
    pub generated_tone_samples: usize,
    pub last_payload_len: Option<usize>,
    pub enabled: bool,
    pub tone_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticRuntimeOptions {
    pub auto_answer: bool,
    pub sco_tx_silence: bool,
    pub sco_tx_tone: bool,
}

impl DiagnosticRuntimeOptions {
    pub fn from_env() -> Self {
        Self {
            auto_answer: hfp_auto_answer_enabled(),
            sco_tx_silence: sco_tx_silence_enabled(),
            sco_tx_tone: sco_tx_tone_enabled(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerAclListenReport {
    pub init: ControllerInitReport,
    pub acl_exchanges: Vec<AclExchangeRecord>,
    pub hfp_events: Vec<HfpEventRecord>,
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerRuntimeListenReport {
    pub init: ControllerInitReport,
    pub events: Vec<ControllerEventRecord>,
    pub acl_exchanges: Vec<AclExchangeRecord>,
    pub hfp_events: Vec<HfpEventRecord>,
    pub hfp_control: HfpControlReport,
    pub sco_audio: ScoAudioReport,
    pub sco_transport: ScoTransportReport,
    pub sco_pcm: ScoPcmReport,
    pub sco_tx: ScoTxReport,
    pub timed_out: bool,
    pub pairing_store_path: String,
    pub stored_link_keys: usize,
}

pub fn initialize_first_controller() -> Result<Option<ControllerInitReport>, String> {
    for interface in winusb::enumerate_hci_radio_interfaces()? {
        match initialize_controller(&interface.path) {
            Ok(report) => return Ok(Some(report)),
            Err(e) => {
                eprintln!(
                    "[AokieRadio] Could not initialize controller at {}: {}",
                    interface.path, e
                );
            }
        }
    }
    Ok(None)
}

pub fn initialize_controller(path: &str) -> Result<ControllerInitReport, String> {
    let transport = AokieHciTransport::open(path)?;
    initialize_transport(path, &transport)
}

pub fn listen_first_controller(
    duration: Duration,
    pairing_store: &mut AokiePairingStore,
) -> Result<Option<ControllerListenReport>, String> {
    for interface in winusb::enumerate_hci_radio_interfaces()? {
        match listen_controller(&interface.path, duration, pairing_store) {
            Ok(report) => return Ok(Some(report)),
            Err(e) => {
                eprintln!(
                    "[AokieRadio] Could not listen on controller at {}: {}",
                    interface.path, e
                );
            }
        }
    }
    Ok(None)
}

pub fn listen_controller(
    path: &str,
    duration: Duration,
    pairing_store: &mut AokiePairingStore,
) -> Result<ControllerListenReport, String> {
    let transport = AokieHciTransport::open(path)?;
    let init = initialize_transport(path, &transport)?;
    // Without per-read timeouts, read_event blocks indefinitely on a
    // quiet bus and the deadline check below never gets re-evaluated —
    // the listener hangs past `duration`. 100 ms ticks line up with
    // listen_runtime_controller_with_options so the cadence is the
    // same across diagnostic helpers.
    transport.set_read_timeouts(100, None, None)?;

    let deadline = Instant::now() + duration;
    let mut events = Vec::new();
    while Instant::now() < deadline {
        match transport.read_event() {
            Ok(packet) => {
                let event = hci::parse_typed_event(&packet)
                    .map_err(|e| format!("HCI event parse error: {}", e))?;
                let action = handle_diagnostic_hci_event(&transport, pairing_store, &event)?;
                events.push(event_record(event, action));
            }
            Err(e) if is_timeout_error(&e) => break,
            Err(e) => return Err(e),
        }
    }

    Ok(ControllerListenReport {
        init,
        events,
        timed_out: true,
        pairing_store_path: pairing_store.path().display().to_string(),
        stored_link_keys: pairing_store.len(),
    })
}

pub fn listen_acl_first_controller(
    duration: Duration,
) -> Result<Option<ControllerAclListenReport>, String> {
    for interface in winusb::enumerate_hci_radio_interfaces()? {
        match listen_acl_controller(&interface.path, duration) {
            Ok(report) => return Ok(Some(report)),
            Err(e) => {
                eprintln!(
                    "[AokieRadio] Could not listen for ACL on controller at {}: {}",
                    interface.path, e
                );
            }
        }
    }
    Ok(None)
}

pub fn listen_acl_controller(
    path: &str,
    duration: Duration,
) -> Result<ControllerAclListenReport, String> {
    let transport = AokieHciTransport::open(path)?;
    let init = initialize_transport(path, &transport)?;
    let max_acl_len = max_acl_packet_len(&init.buffer_size);
    // Same reason as listen_controller: read_acl blocks until data
    // arrives, so we need a polling timeout on top of the deadline
    // loop or the listener never honours `duration` on a quiet bus.
    transport.set_read_timeouts(100, Some(100), None)?;

    let deadline = Instant::now() + duration;
    let mut l2cap_state = l2cap::L2capState::new();
    let mut acl_exchanges = Vec::new();
    let mut hfp_events = Vec::new();

    while Instant::now() < deadline {
        match transport.read_acl(max_acl_len) {
            Ok(packet) => {
                let responses = l2cap_state.handle_acl_packet(&packet)?;
                let response_lengths = responses.iter().map(Vec::len).collect::<Vec<_>>();
                for response in &responses {
                    transport.write_acl(response)?;
                }
                acl_exchanges.push(AclExchangeRecord {
                    inbound_len: packet.len(),
                    responses_sent: responses.len(),
                    response_lengths,
                });
                hfp_events.extend(
                    l2cap_state
                        .take_hfp_events()
                        .into_iter()
                        .map(hfp_event_record),
                );
            }
            Err(e) if is_timeout_error(&e) => break,
            Err(e) => return Err(e),
        }
    }

    Ok(ControllerAclListenReport {
        init,
        acl_exchanges,
        hfp_events,
        timed_out: true,
    })
}

pub fn listen_runtime_first_controller(
    duration: Duration,
    pairing_store: &mut AokiePairingStore,
) -> Result<Option<ControllerRuntimeListenReport>, String> {
    listen_runtime_first_controller_with_options(
        duration,
        pairing_store,
        DiagnosticRuntimeOptions::from_env(),
    )
}

pub fn listen_runtime_first_controller_with_options(
    duration: Duration,
    pairing_store: &mut AokiePairingStore,
    options: DiagnosticRuntimeOptions,
) -> Result<Option<ControllerRuntimeListenReport>, String> {
    for interface in winusb::enumerate_hci_radio_interfaces()? {
        match listen_runtime_controller_with_options(
            &interface.path,
            duration,
            pairing_store,
            options,
        ) {
            Ok(report) => return Ok(Some(report)),
            Err(e) => {
                eprintln!(
                    "[AokieRadio] Could not run runtime diagnostic on controller at {}: {}",
                    interface.path, e
                );
            }
        }
    }
    Ok(None)
}

pub fn listen_runtime_controller(
    path: &str,
    duration: Duration,
    pairing_store: &mut AokiePairingStore,
) -> Result<ControllerRuntimeListenReport, String> {
    listen_runtime_controller_with_options(
        path,
        duration,
        pairing_store,
        DiagnosticRuntimeOptions::from_env(),
    )
}

pub fn listen_runtime_controller_with_options(
    path: &str,
    duration: Duration,
    pairing_store: &mut AokiePairingStore,
    options: DiagnosticRuntimeOptions,
) -> Result<ControllerRuntimeListenReport, String> {
    let mut transport = AokieHciTransport::open(path)?;
    let init = initialize_transport(path, &transport)?;
    transport.set_read_timeouts(100, Some(100), Some(100))?;

    let max_acl_len = max_acl_packet_len(&init.buffer_size);
    let max_sco_len = max_sco_packet_len(&init.buffer_size);
    let deadline = Instant::now() + duration;
    let mut l2cap_state = l2cap::L2capState::new();
    let mut sco_stats = sco::ScoStats::default();
    let mut pcm_stats = sco::LinearPcmStats::default();
    let mut sco_assembler = sco::ScoPacketAssembler::new();
    let mut sco_tx_queue = sco::LinearPcmTxQueue::new(AOKIE_SCO_TX_QUEUE_SAMPLES);
    let sco_tx_tone_enabled = options.sco_tx_tone;
    let sco_tx_enabled = options.sco_tx_silence || sco_tx_tone_enabled;
    let auto_answer_enabled = options.auto_answer;
    let mut sco_tx_report = ScoTxReport {
        enabled: sco_tx_enabled,
        tone_enabled: sco_tx_tone_enabled,
        ..Default::default()
    };
    let mut hfp_control = HfpControlReport {
        auto_answer_enabled,
        ..Default::default()
    };
    let mut answer_sent_for_call = false;
    let mut sco_tx_tone_phase = 0usize;
    let mut selected_codec = None;
    let mut active_sco_handle = None;
    let mut events = Vec::new();
    let mut acl_exchanges = Vec::new();
    let mut hfp_events = Vec::new();

    while Instant::now() < deadline {
        match transport.read_event() {
            Ok(packet) => {
                let event = hci::parse_typed_event(&packet)
                    .map_err(|e| format!("HCI event parse error: {}", e))?;
                let action = handle_diagnostic_hci_event(&transport, pairing_store, &event)?;
                if let hci::HciEvent::SynchronousConnectionComplete {
                    status,
                    connection_handle,
                    ..
                } = &event
                {
                    if *status == 0 {
                        active_sco_handle = Some(*connection_handle);
                        transport.configure_sco_alt_setting(AOKIE_VOICE_SETTING, 1)?;
                    }
                }
                if let hci::HciEvent::DisconnectionComplete {
                    status,
                    connection_handle,
                    ..
                } = &event
                {
                    if *status == 0 {
                        l2cap_state.remove_connection(*connection_handle);
                        if active_sco_handle == Some(*connection_handle) {
                            active_sco_handle = None;
                            transport.disable_sco_alt_setting()?;
                        }
                    }
                }
                events.push(event_record(event, action));
            }
            Err(e) if is_timeout_error(&e) => {}
            Err(e) => return Err(e),
        }

        match transport.read_acl(max_acl_len) {
            Ok(packet) => {
                let responses = l2cap_state.handle_acl_packet(&packet)?;
                let response_lengths = responses.iter().map(Vec::len).collect::<Vec<_>>();
                for response in &responses {
                    transport.write_acl(response)?;
                }
                acl_exchanges.push(AclExchangeRecord {
                    inbound_len: packet.len(),
                    responses_sent: responses.len(),
                    response_lengths,
                });
                for event in l2cap_state.take_hfp_events() {
                    update_selected_codec(&mut selected_codec, &event);
                    // Codec selection is sticky across CallTerminated:
                    // Pixel/Bluedroid caches the negotiated codec and on
                    // back-to-back calls fires the next SCO request
                    // *before* sending +BCS, expecting the prior link
                    // parameters. Resetting here would put us in the
                    // CVSD don't-care branch while the peer wanted
                    // mSBC/T2, leading to Connection_Accept_Timeout
                    // until the phone gives up its cache. Mirrors the
                    // main runtime — the cache only clears on ACL drop.
                    let control_packets = hfp_call_control_packets_for_event(
                        &mut l2cap_state,
                        &event,
                        &mut answer_sent_for_call,
                        &mut hfp_control,
                    )?;
                    for packet in &control_packets {
                        transport.write_acl(packet)?;
                    }
                    hfp_events.push(hfp_event_record(event));
                }
            }
            Err(e) if is_timeout_error(&e) => {}
            Err(e) => return Err(e),
        }

        if active_sco_handle.is_some() {
            match transport.read_sco(max_sco_len.min(255)) {
                Ok(bytes) => {
                    for packet in sco_assembler.push_bytes(&bytes)? {
                        let _ = sco_stats.record_packet(&packet);
                        if selected_codec
                            .as_ref()
                            .map(|codec| codec.0 == "CVSD")
                            .unwrap_or(true)
                        {
                            let _ = pcm_stats.record_sco_packet(&packet);
                        }
                    }
                }
                Err(e) if is_timeout_error(&e) || e.contains("no HCI SCO in endpoint") => {}
                Err(e) => return Err(e),
            }

            if sco_tx_enabled
                && selected_codec
                    .as_ref()
                    .map(|codec| codec.0 == "CVSD")
                    .unwrap_or(true)
            {
                if let Some(handle) = active_sco_handle {
                    // Match the production path: 48-byte payload (24
                    // samples = 3 ms audio) is the BTstack-equivalent
                    // USB-aligned size (51-byte HCI packet = 3 × 17
                    // alt-2 frames) that avoids the chronic underrun
                    // we hit at 60 bytes.
                    let payload_len = max_sco_payload_len(&init.buffer_size)
                        .min(AOKIE_SCO_USB_PAYLOAD_BYTES)
                        & !1;
                    if payload_len > 0 {
                        if sco_tx_tone_enabled {
                            sco_tx_report.generated_tone_samples += fill_sco_tx_test_tone(
                                &mut sco_tx_queue,
                                &mut sco_tx_tone_phase,
                                payload_len / 2,
                            );
                        }
                        let had_audio = !sco_tx_queue.is_empty();
                        let packet = sco_tx_queue.pop_sco_packet(handle, payload_len)?;
                        transport.write_sco(&packet)?;
                        sco_tx_report.packets += 1;
                        sco_tx_report.payload_bytes += payload_len;
                        sco_tx_report.silence_packets += usize::from(!had_audio);
                        sco_tx_report.last_payload_len = Some(payload_len);
                    }
                }
            }
        }
    }

    sco_tx_report.queued_samples = sco_tx_queue.len();
    sco_tx_report.dropped_samples = sco_tx_queue.dropped_samples();

    Ok(ControllerRuntimeListenReport {
        init,
        events,
        acl_exchanges,
        hfp_events,
        hfp_control,
        sco_audio: sco_audio_report(sco_stats, active_sco_handle),
        sco_transport: sco_transport_report(transport.sco_transport_config()),
        sco_pcm: sco_pcm_report(pcm_stats, selected_codec),
        sco_tx: sco_tx_report,
        timed_out: true,
        pairing_store_path: pairing_store.path().display().to_string(),
        stored_link_keys: pairing_store.len(),
    })
}

pub(crate) fn initialize_transport(
    path: &str,
    transport: &AokieHciTransport,
) -> Result<ControllerInitReport, String> {
    // Drop any URBs the kernel buffered or has in flight on our IN
    // pipes from a previous session. Without this, an HCI ACL packet
    // queued by the controller while a prior runtime owned the WinUSB
    // handle (or that landed after our previous WinUsb_Free) gets
    // returned at the start of the next read — our parser sees garbage
    // prefix bytes ahead of the next real frame, which manifests as
    // ConfigureRequest-stall pairing failures right after app start
    // (only "fixed" by physically replugging the dongle).
    transport.flush_in_pipes()?;
    transport.reset()?;
    transport.set_event_mask(AOKIE_EVENT_MASK)?;

    let version = transport.read_local_version()?;
    let features = transport.read_local_supported_features()?;
    let buffer_size = transport.read_buffer_size()?;
    let local_address = transport.read_bd_addr()?;

    // Broadcom vendor init: route SCO over the HCI iso transport instead
    // of the PCM/I2S pins. BTStack `hci.c:2241-2248` only fires this when
    // `manufacturer == BLUETOOTH_COMPANY_ID_BROADCOM_CORPORATION`. Without
    // it, BCM20702A0 silently routes SCO to its (unconnected) PCM pins
    // and every HCI iso URB completes with zero bytes — same controller
    // family that worked under BTStack-bridge but went silent on our port
    // until we discovered this missing init step (2026-04-29).
    if version.manufacturer_name == hci::BLUETOOTH_COMPANY_ID_BROADCOM {
        eprintln!(
            "[AokieRadio] Broadcom controller detected (manufacturer 0x{:04x}) — routing SCO via HCI iso (BCM_WRITE_SCO_PCM_INT 1,0,0,0,0)",
            version.manufacturer_name,
        );
        command_status(
            transport,
            &hci::write_bcm_sco_pcm_int_command(1, 0, 0, 0, 0),
            hci::OPCODE_BCM_WRITE_SCO_PCM_INT,
            "BCM Write SCO PCM Interface (route SCO via HCI)",
        )?;
    }

    command_status(
        transport,
        &hci::write_local_name_command(AOKIE_CLASSIC_NAME),
        hci::OPCODE_WRITE_LOCAL_NAME,
        "Write Local Name",
    )?;
    command_status(
        transport,
        &hci::write_class_of_device_command(AOKIE_CLASS_OF_DEVICE),
        hci::OPCODE_WRITE_CLASS_OF_DEVICE,
        "Write Class of Device",
    )?;
    command_status(
        transport,
        &hci::write_page_timeout_command(AOKIE_PAGE_TIMEOUT),
        hci::OPCODE_WRITE_PAGE_TIMEOUT,
        "Write Page Timeout",
    )?;
    command_status(
        transport,
        &hci::write_default_link_policy_settings_command(AOKIE_LINK_POLICY),
        hci::OPCODE_WRITE_DEFAULT_LINK_POLICY_SETTINGS,
        "Write Default Link Policy Settings",
    )?;
    command_status(
        transport,
        &hci::write_voice_setting_command(AOKIE_VOICE_SETTING),
        hci::OPCODE_WRITE_VOICE_SETTING,
        "Write Voice Setting",
    )?;
    command_status(
        transport,
        &hci::write_simple_pairing_mode_command(true),
        hci::OPCODE_WRITE_SIMPLE_PAIRING_MODE,
        "Write Simple Pairing Mode",
    )?;
    // EIR has to land before scanning goes "discoverable" so the very
    // first inquiry response we emit already advertises MAP/PBAP UUIDs.
    let eir_command =
        hci::write_extended_inquiry_response_command(false, AOKIE_CLASSIC_NAME, AOKIE_EIR_UUIDS);
    eprintln!(
        "[AokieRadio] Write EIR — name={:?} uuids={:?} (first 24 bytes={:02x?})",
        AOKIE_CLASSIC_NAME,
        AOKIE_EIR_UUIDS,
        &eir_command[..24]
    );
    command_status(
        transport,
        &eir_command,
        hci::OPCODE_WRITE_EXTENDED_INQUIRY_RESPONSE,
        "Write Extended Inquiry Response",
    )?;

    // Flush BEFORE scan_enable: the opening flush_in_pipes() clears the
    // pre-session ring, but setup commands between there and here
    // (notably Broadcom 21ec's BCM_WRITE_SCO_PCM_INT) can leave residue
    // — symptom is a 2-byte `88 e0 ...` prefix on the first ACL packet
    // of the next connection. Doing this AFTER scan_enable would race a
    // fast-connecting peer's first L2CAP InformationRequest. Event pipe
    // stays untouched; the deferred-event queue de-dups Command_Complete
    // acks we've already consumed.
    transport.flush_acl_in_pipe()?;

    // AOK-BT-001: come up CONNECTABLE-ONLY (bonded phones reconnect, strangers
    // can't discover/pair us). The runtime opens a discoverable pairing window
    // on demand (phone.startPairing) and closes it on success/cancel/timeout.
    command_status(
        transport,
        &hci::write_scan_enable_command(AOKIE_SCAN_ENABLE_CONNECTABLE),
        hci::OPCODE_WRITE_SCAN_ENABLE,
        "Write Scan Enable",
    )?;

    Ok(ControllerInitReport {
        device_path: path.to_string(),
        local_address,
        version,
        features,
        buffer_size,
        local_name: AOKIE_CLASSIC_NAME.to_string(),
        class_of_device: AOKIE_CLASS_OF_DEVICE,
        page_timeout: AOKIE_PAGE_TIMEOUT,
        link_policy: AOKIE_LINK_POLICY,
        voice_setting: AOKIE_VOICE_SETTING,
        scan_enable: AOKIE_SCAN_ENABLE_CONNECTABLE,
        simple_pairing_enabled: true,
    })
}

fn command_status(
    transport: &AokieHciTransport,
    command: &[u8],
    opcode: u16,
    operation: &str,
) -> Result<(), String> {
    let params = transport.command_return_params(command, opcode)?;
    hci::expect_status_ok(&params, operation)
}

/// AOK-BT-001 admission gate — pure so the policy is unit-testable without a
/// live radio. `is_bonded` = we already hold a stored link key for the device;
/// `pairing_open` = an explicit pairing window is currently open.
///
/// An incoming ACL connection is accepted when the device is already bonded
/// (normal reconnect, always allowed) OR the pairing window is open (a new
/// device is deliberately being added). An unknown device at rest is refused.
pub fn may_accept_acl_connection(is_bonded: bool, pairing_open: bool) -> bool {
    is_bonded || pairing_open
}

/// AOK-BT-001 admission gate for a FRESH bond — the SSP/PIN reply steps and the
/// LinkKeyNotification store that together create a new bond. Allowed only while
/// the pairing window is open; a stranger can never complete a bond at rest.
pub fn may_complete_new_bond(pairing_open: bool) -> bool {
    pairing_open
}

/// Write the controller's scan-enable (audit AOK-BT-001): the runtime loop uses
/// this to flip between discoverable-connectable (pairing window open) and
/// connectable-only (at rest).
pub fn write_scan_enable(transport: &AokieHciTransport, scan_enable: u8) -> Result<(), String> {
    command_status(
        transport,
        &hci::write_scan_enable_command(scan_enable),
        hci::OPCODE_WRITE_SCAN_ENABLE,
        "Write Scan Enable",
    )
}

/// PAIR-001: how the pairing handshake steps are answered. Groups the
/// AOK-BT-001 window gate with the two authentication policies the audit
/// added so every call site states its full posture explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingPolicy {
    /// AOK-BT-001: an explicit pairing window is currently open.
    pub pairing_open: bool,
    /// Legacy PIN ("0000") pairing for pre-SSP peers. DEFAULT OFF — a fixed
    /// well-known PIN authenticates nothing, so it only exists behind the
    /// explicit `legacyPairingPin` compat setting, and even then only inside
    /// an open pairing window.
    pub legacy_pin_allowed: bool,
    /// true = production runtime: SSP runs as NUMERIC COMPARISON
    /// (DisplayYesNo + MITM) and the UserConfirmationRequest is HELD for the
    /// operator instead of auto-accepted; the caller must surface
    /// `PairingOutcome::ConfirmationPending` to a human and later send the
    /// (negative) confirmation reply itself.
    /// false = diagnostic CLIs: just-works auto-accept (the CLI invocation
    /// IS the explicit operator action).
    pub operator_confirm: bool,
}

impl PairingPolicy {
    /// Diagnostic tools (`aokie-dongle` listen/call CLIs): running them is an
    /// explicit operator action, so the window is treated as open and SSP is
    /// auto-confirmed. Legacy PIN stays available for bench dongles.
    pub fn diagnostic() -> Self {
        Self {
            pairing_open: true,
            legacy_pin_allowed: true,
            operator_confirm: false,
        }
    }

    /// The production radio runtime: window state comes from the live
    /// `PairingWindow`, legacy PIN from the `legacyPairingPin` setting, and
    /// numeric confirmation always goes to the operator.
    pub fn runtime(pairing_open: bool, legacy_pin_allowed: bool) -> Self {
        Self {
            pairing_open,
            legacy_pin_allowed,
            operator_confirm: true,
        }
    }
}

/// What a pairing-related HCI event resolved to (PAIR-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingOutcome {
    /// Handled synchronously — the string is the log line.
    Logged(String),
    /// SSP numeric comparison is waiting on the operator: NO HCI reply has
    /// been sent. The caller shows `numeric_value` next to the phone's
    /// display and answers with `user_confirmation_request_reply_command`
    /// (accept) or `user_confirmation_request_negative_reply_command`
    /// (reject / timeout) — never silently.
    ConfirmationPending { address: String, numeric_value: u32 },
}

fn handle_pairing_event(
    transport: &AokieHciTransport,
    pairing_store: &mut AokiePairingStore,
    event: &hci::HciEvent,
    policy: PairingPolicy,
) -> Result<Option<PairingOutcome>, String> {
    let pairing_open = policy.pairing_open;
    let logged = |s: String| Ok(Some(PairingOutcome::Logged(s)));
    match event {
        hci::HciEvent::LinkKeyRequest { address } => {
            if let Some(record) = pairing_store.get(address)? {
                let command = hci::link_key_request_reply_command(address, &record.link_key)?;
                command_status(
                    transport,
                    &command,
                    hci::OPCODE_LINK_KEY_REQUEST_REPLY,
                    "Link Key Request Reply",
                )?;
                logged(format!(
                    "sent stored link-key reply type 0x{:02x}",
                    record.key_type
                ))
            } else {
                let command = hci::link_key_request_negative_reply_command(address)?;
                command_status(
                    transport,
                    &command,
                    hci::OPCODE_LINK_KEY_REQUEST_NEGATIVE_REPLY,
                    "Link Key Request Negative Reply",
                )?;
                logged("sent negative link-key reply (no stored key)".to_string())
            }
        }
        hci::HciEvent::LinkKeyNotification {
            address,
            link_key,
            key_type,
        } => {
            // Only persist a NEW bond while the pairing window is open (AOK-BT-001).
            // A reconnecting bonded device uses LinkKeyRequest→reply and never emits
            // a notification, so refusing here only rejects an unsanctioned fresh bond.
            if !may_complete_new_bond(pairing_open) {
                return logged("refused link-key store — no pairing window open".to_string());
            }
            pairing_store.put(address, *link_key, *key_type)?;
            logged(format!("stored link key type 0x{:02x}", key_type))
        }
        hci::HciEvent::PinCodeRequest { address } => {
            // Legacy PIN pairing — a fresh bond; gated on the window (AOK-BT-001)
            // AND on the explicit compat setting (PAIR-001): a fixed "0000" PIN
            // authenticates nothing, so it is refused unless deliberately enabled.
            if !may_complete_new_bond(pairing_open) || !policy.legacy_pin_allowed {
                let command = hci::pin_code_request_negative_reply_command(address)?;
                command_status(
                    transport,
                    &command,
                    hci::OPCODE_PIN_CODE_REQUEST_NEGATIVE_REPLY,
                    "PIN Code Request Negative Reply",
                )?;
                return logged(if may_complete_new_bond(pairing_open) {
                    "refused legacy PIN pairing — legacyPairingPin compat mode is off (PAIR-001)"
                        .to_string()
                } else {
                    "refused PIN pairing — no pairing window open".to_string()
                });
            }
            eprintln!(
                "[AokieRadio] ⚠️ legacy PIN pairing in use for {} — the fixed \"{}\" PIN \
                 provides NO authentication; disable legacyPairingPin once the device is bonded",
                address, AOKIE_LEGACY_PIN
            );
            let command = hci::pin_code_request_reply_command(address, AOKIE_LEGACY_PIN)?;
            command_status(
                transport,
                &command,
                hci::OPCODE_PIN_CODE_REQUEST_REPLY,
                "PIN Code Request Reply",
            )?;
            logged(format!("sent legacy PIN reply {}", AOKIE_LEGACY_PIN))
        }
        hci::HciEvent::IoCapabilityRequest { address } => {
            // Simple Pairing IO-capability exchange — a fresh bond; gated (AOK-BT-001).
            // Reason 0x18 = "Pairing Not Allowed".
            if !may_complete_new_bond(pairing_open) {
                let command = hci::io_capability_request_negative_reply_command(address, 0x18)?;
                command_status(
                    transport,
                    &command,
                    hci::OPCODE_IO_CAPABILITY_REQUEST_NEGATIVE_REPLY,
                    "IO Capability Request Negative Reply",
                )?;
                return logged("refused Simple Pairing — no pairing window open".to_string());
            }
            // PAIR-001: in production the Desktop UI is our display, so we
            // advertise DisplayYesNo + MITM — SSP resolves to numeric
            // comparison and a stranger can't complete a silent just-works
            // bond during the window. Diagnostic CLIs keep just-works.
            let (io_capability, authreq, label) = if policy.operator_confirm {
                (
                    hci::SSP_IO_CAPABILITY_DISPLAY_YES_NO,
                    hci::SSP_AUTHREQ_MITM_REQUIRED_GENERAL_BONDING,
                    "sent DisplayYesNo + MITM-required IO capability reply (numeric comparison)",
                )
            } else {
                (
                    hci::SSP_IO_CAPABILITY_NO_INPUT_NO_OUTPUT,
                    hci::SSP_AUTHREQ_MITM_NOT_REQUIRED_GENERAL_BONDING,
                    "sent no-input/no-output IO capability reply",
                )
            };
            let command = hci::io_capability_request_reply_command(
                address,
                io_capability,
                hci::SSP_OOB_DATA_NOT_PRESENT,
                authreq,
            )?;
            command_status(
                transport,
                &command,
                hci::OPCODE_IO_CAPABILITY_REQUEST_REPLY,
                "IO Capability Request Reply",
            )?;
            logged(label.to_string())
        }
        hci::HciEvent::UserConfirmationRequest {
            address,
            numeric_value,
        } => {
            // The confirmation that seals an SSP bond; gated (AOK-BT-001).
            if !may_complete_new_bond(pairing_open) {
                let command = hci::user_confirmation_request_negative_reply_command(address)?;
                command_status(
                    transport,
                    &command,
                    hci::OPCODE_USER_CONFIRMATION_REQUEST_NEGATIVE_REPLY,
                    "User Confirmation Request Negative Reply",
                )?;
                return logged(
                    "refused Simple Pairing confirmation — no pairing window open".to_string(),
                );
            }
            if policy.operator_confirm {
                // PAIR-001: HOLD the reply — the operator must compare the
                // numeric code against the phone's display and answer via the
                // Desktop UI. The caller owns sending the eventual reply.
                return Ok(Some(PairingOutcome::ConfirmationPending {
                    address: address.clone(),
                    numeric_value: *numeric_value,
                }));
            }
            let command = hci::user_confirmation_request_reply_command(address)?;
            command_status(
                transport,
                &command,
                hci::OPCODE_USER_CONFIRMATION_REQUEST_REPLY,
                "User Confirmation Request Reply",
            )?;
            logged("auto-confirmed Simple Pairing request".to_string())
        }
        _ => Ok(None),
    }
}

pub(crate) fn handle_diagnostic_hci_event(
    transport: &AokieHciTransport,
    pairing_store: &mut AokiePairingStore,
    event: &hci::HciEvent,
) -> Result<Option<String>, String> {
    // Diagnostic sessions (the `aokie-dongle` listen/call tools) are an EXPLICIT
    // operator action to exercise pairing: window treated as open, legacy PIN
    // available, SSP auto-confirmed (PairingPolicy::diagnostic()).
    Ok(handle_hci_event_with_policy(
        transport,
        pairing_store,
        event,
        None,
        PairingPolicy::diagnostic(),
    )?
    .map(|outcome| match outcome {
        PairingOutcome::Logged(action) => action,
        // Unreachable with operator_confirm=false, but stay total.
        PairingOutcome::ConfirmationPending { address, .. } => {
            format!("pairing confirmation pending for {}", address)
        }
    }))
}

pub(crate) fn handle_hci_event_with_policy(
    transport: &AokieHciTransport,
    pairing_store: &mut AokiePairingStore,
    event: &hci::HciEvent,
    selected_codec: Option<&(String, u16)>,
    policy: PairingPolicy,
) -> Result<Option<PairingOutcome>, String> {
    if let Some(outcome) = handle_pairing_event(transport, pairing_store, event, policy)? {
        return Ok(Some(outcome));
    }
    let pairing_open = policy.pairing_open;
    let logged = |s: String| Ok(Some(PairingOutcome::Logged(s)));

    match event {
        hci::HciEvent::ConnectionRequest {
            address,
            link_type: hci::LINK_TYPE_ACL,
            ..
        } => {
            // AOK-BT-001: at rest, only a BONDED device may reconnect; an unknown
            // device is accepted only inside an explicit pairing window, else rejected.
            let is_bonded = pairing_store.contains(address);
            if !may_accept_acl_connection(is_bonded, pairing_open) {
                let command = hci::reject_connection_request_command(address)?;
                transport.write_command(&command)?;
                return logged(
                    "rejected incoming ACL from unknown device (no pairing window open)"
                        .to_string(),
                );
            }
            let command =
                hci::accept_connection_request_command(address, hci::ACCEPT_ROLE_REMAIN_SLAVE)?;
            transport.write_command(&command)?;
            logged("accepted incoming ACL connection request".to_string())
        }
        hci::HciEvent::ConnectionRequest {
            address, link_type, ..
        } if matches!(*link_type, hci::LINK_TYPE_SCO | hci::LINK_TYPE_ESCO) => {
            let params = sco_accept_parameters(*link_type, selected_codec);
            let command = hci::accept_synchronous_connection_request_command(
                address,
                params.transmit_bandwidth,
                params.receive_bandwidth,
                params.max_latency,
                params.voice_setting,
                params.retransmission_effort,
                params.packet_types,
            )?;
            transport.write_command(&command)?;
            logged(format!(
                "accepted incoming SCO/eSCO connection request ({})",
                params.codec_label
            ))
        }
        hci::HciEvent::ConnectionRequest { link_type, .. } => logged(format!(
            "ignored unsupported connection request link type {}",
            link_type
        )),
        _ => Ok(None),
    }
}

fn event_record(event: hci::HciEvent, action: Option<String>) -> ControllerEventRecord {
    ControllerEventRecord {
        event_code: event.event_code(),
        name: event.name().to_string(),
        summary: event.summary(),
        action,
    }
}

fn hfp_event_record(event: hfp::HfpEvent) -> HfpEventRecord {
    match event {
        hfp::HfpEvent::ServiceLevelConnectionReady => HfpEventRecord {
            name: "ServiceLevelConnectionReady".to_string(),
            summary: "HFP service-level connection ready".to_string(),
        },
        hfp::HfpEvent::ServiceLevelConnectionFailed(reason) => HfpEventRecord {
            name: "ServiceLevelConnectionFailed".to_string(),
            summary: format!("HFP SLC failed at {}", reason),
        },
        hfp::HfpEvent::IncomingCall => HfpEventRecord {
            name: "IncomingCall".to_string(),
            summary: "incoming call setup".to_string(),
        },
        hfp::HfpEvent::Ringing => HfpEventRecord {
            name: "Ringing".to_string(),
            summary: "ring notification".to_string(),
        },
        hfp::HfpEvent::OutgoingDialing => HfpEventRecord {
            name: "OutgoingDialing".to_string(),
            summary: "outbound call setup started".to_string(),
        },
        hfp::HfpEvent::CallAnswered => HfpEventRecord {
            name: "CallAnswered".to_string(),
            summary: "call indicator active".to_string(),
        },
        hfp::HfpEvent::CallTerminated => HfpEventRecord {
            name: "CallTerminated".to_string(),
            summary: "call indicator inactive".to_string(),
        },
        hfp::HfpEvent::CallerId(number) => HfpEventRecord {
            name: "CallerId".to_string(),
            summary: number,
        },
        hfp::HfpEvent::CallWaiting(number) => HfpEventRecord {
            name: "CallWaiting".to_string(),
            summary: number.unwrap_or_else(|| "number not yet known".to_string()),
        },
        hfp::HfpEvent::CallWaitingEnded => HfpEventRecord {
            name: "CallWaitingEnded".to_string(),
            summary: "waiting caller gone, active call untouched".to_string(),
        },
        hfp::HfpEvent::CallHeld(state) => HfpEventRecord {
            name: "CallHeld".to_string(),
            summary: format!("callheld indicator {}", state),
        },
        hfp::HfpEvent::CallListEntry(entry) => HfpEventRecord {
            name: "CallListEntry".to_string(),
            summary: format!(
                "idx={} dir={} status={} mpty={}",
                entry.index, entry.direction, entry.status, entry.multiparty
            ),
        },
        hfp::HfpEvent::CodecSelected { codec, sample_rate } => HfpEventRecord {
            name: "CodecSelected".to_string(),
            summary: format!("{}:{}", codec, sample_rate),
        },
    }
}

pub(crate) fn update_selected_codec(
    selected_codec: &mut Option<(String, u16)>,
    event: &hfp::HfpEvent,
) {
    if let hfp::HfpEvent::CodecSelected { codec, sample_rate } = event {
        *selected_codec = Some((codec.clone(), *sample_rate));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScoAcceptParameters {
    pub transmit_bandwidth: u32,
    pub receive_bandwidth: u32,
    pub max_latency: u16,
    pub voice_setting: u16,
    pub retransmission_effort: u8,
    pub packet_types: u16,
    pub codec_label: &'static str,
}

/// Pick HCI Accept_Synchronous_Connection_Request parameters that match
/// the negotiated codec and the link type the AG asked for.
///
/// - mSBC over eSCO: HFP §5.7.1.4 T2 parameters (max_latency=13ms,
///   transparent voice setting, retransmission_effort=2, EV3 + 2-EV3
///   packets allowed).
/// - CVSD over eSCO: legacy "don't-care" path that lets the controller
///   negotiate something reasonable for narrow-band speech (we don't
///   pin S4 explicitly so older controllers can fall back to S1).
/// - Legacy SCO link: always CVSD with HV1/HV3 packet types, regardless
///   of negotiated codec — mSBC needs eSCO and the AG should never have
///   asked for an HV link if it picked mSBC.
pub(crate) fn sco_accept_parameters(
    link_type: u8,
    selected_codec: Option<&(String, u16)>,
) -> ScoAcceptParameters {
    let is_msbc = selected_codec
        .map(|(codec, _)| codec.eq_ignore_ascii_case("mSBC"))
        .unwrap_or(false);

    if link_type == hci::LINK_TYPE_ESCO && is_msbc {
        return ScoAcceptParameters {
            transmit_bandwidth: 8000,
            receive_bandwidth: 8000,
            max_latency: HFP_T2_MAX_LATENCY_MS,
            voice_setting: AOKIE_VOICE_SETTING_TRANSPARENT,
            retransmission_effort: HFP_T2_RETRANSMISSION_EFFORT,
            packet_types: hci::SCO_PACKET_TYPES_HFP_CVSD_ESCO_COMMAND,
            codec_label: "mSBC/T2",
        };
    }

    ScoAcceptParameters {
        transmit_bandwidth: 8000,
        receive_bandwidth: 8000,
        max_latency: 0xffff,
        voice_setting: AOKIE_VOICE_SETTING,
        retransmission_effort: hci::SCO_RETRANSMISSION_EFFORT_DONT_CARE,
        packet_types: if link_type == hci::LINK_TYPE_ESCO {
            hci::SCO_PACKET_TYPES_HFP_CVSD_ESCO_COMMAND
        } else {
            hci::SCO_PACKET_TYPES_HFP_CVSD_SCO_COMMAND
        },
        codec_label: "CVSD",
    }
}

pub(crate) fn hfp_call_control_packets_for_event(
    l2cap_state: &mut l2cap::L2capState,
    event: &hfp::HfpEvent,
    answer_sent_for_call: &mut bool,
    report: &mut HfpControlReport,
) -> Result<Vec<Vec<u8>>, String> {
    match event {
        hfp::HfpEvent::CallTerminated => {
            *answer_sent_for_call = false;
            Ok(Vec::new())
        }
        hfp::HfpEvent::CallAnswered => {
            *answer_sent_for_call = true;
            // Caller-id rescue (2026-07-13): instant auto-answer races the
            // ringing-phase +CLIP — several live calls ended with NO caller
            // id at all. AT+CLCC returns the active call's number
            // deterministically; its +CLCC response feeds the same CallerId
            // pipeline as +CLIP, so the plugin/session/records pick it up
            // exactly as if the +CLIP had landed.
            let packets =
                l2cap_state.build_hfp_call_control_packets(hfp::HfpAtCommand::ListCurrentCalls)?;
            if !packets.is_empty() {
                report.last_action = Some("sent AT+CLCC caller-id query".to_string());
            }
            Ok(packets)
        }
        hfp::HfpEvent::CallWaiting(_) => {
            // Phase 4 observe-only topology: every waiting knock triggers a
            // fresh AT+CLCC so the AUTHORITATIVE call list (the active leg +
            // the status-5 waiting leg, with indexes) lands in the logs —
            // the soak data the switchboard slice's index handling needs.
            // Read-only command, same mid-call safety as the caller-id
            // rescue on CallAnswered above.
            let packets =
                l2cap_state.build_hfp_call_control_packets(hfp::HfpAtCommand::ListCurrentCalls)?;
            if !packets.is_empty() {
                report.last_action =
                    Some("sent AT+CLCC topology query for a waiting call".to_string());
            }
            Ok(packets)
        }
        hfp::HfpEvent::IncomingCall | hfp::HfpEvent::Ringing
            if report.auto_answer_enabled && !*answer_sent_for_call =>
        {
            report.answer_attempts += 1;
            let packets = l2cap_state.build_hfp_call_control_packets(hfp::HfpAtCommand::Answer)?;
            if packets.is_empty() {
                report.last_action = Some("could not auto-answer: no open HFP channel".to_string());
            } else {
                *answer_sent_for_call = true;
                report.answer_packets_sent += packets.len();
                report.last_action = Some("sent HFP answer command".to_string());
            }
            Ok(packets)
        }
        _ => Ok(Vec::new()),
    }
}

pub(crate) fn max_acl_packet_len(buffer_size: &hci::BufferSize) -> usize {
    // The controller's `acl_data_packet_length` is the host→controller
    // ceiling — what the host is allowed to send. The controller→host
    // direction is *not* constrained by Read_Buffer_Size and varies by
    // chipset; some Bluedroid pairings deliver inbound ACL packets up
    // to ~4 KB (e.g. an OBEX bMessage GET response that the peer
    // didn't fragment at L2CAP because the negotiated MTU was high).
    //
    // WinUSB defaults are ALLOW_PARTIAL_READS=TRUE / AUTO_FLUSH=FALSE,
    // which means an inbound packet larger than our read buffer is
    // returned in two chunks — the second of which our parser would
    // misinterpret as a fresh ACL header (length field = mid-payload
    // garbage, frame dropped, runtime crashes on the propagated
    // error). Sizing the read buffer at >= 4 KB practically eliminates
    // that case for Bluetooth Classic traffic; the extra few KB of
    // heap per read is well below the noise.
    let advertised = 4 + buffer_size.acl_data_packet_length as usize;
    advertised.max(4096)
}

pub(crate) fn max_sco_packet_len(buffer_size: &hci::BufferSize) -> usize {
    let payload_len = buffer_size.sco_data_packet_length as usize;
    if payload_len == 0 {
        255
    } else {
        3 + payload_len
    }
}

pub(crate) fn max_sco_payload_len(buffer_size: &hci::BufferSize) -> usize {
    let payload_len = buffer_size.sco_data_packet_length as usize;
    if payload_len == 0 {
        252
    } else {
        payload_len
    }
}

fn sco_audio_report(stats: sco::ScoStats, active_connection_handle: Option<u16>) -> ScoAudioReport {
    ScoAudioReport {
        packets: stats.packets,
        payload_bytes: stats.payload_bytes,
        bad_packets: stats.bad_packets,
        active_connection_handle,
        last_connection_handle: stats.last_connection_handle,
        last_packet_status_flag: stats.last_packet_status_flag,
    }
}

fn sco_transport_report(config: Option<winusb::ScoTransportConfig>) -> ScoTransportReport {
    config
        .map(|config| ScoTransportReport {
            alternate_setting: Some(config.alternate_setting),
            in_pipe_id: config.in_pipe_id,
            out_pipe_id: config.out_pipe_id,
            max_packet_size: config.max_packet_size,
            isoch_buffer_len: config.isoch_buffer_len,
            isoch_buffers_registered: config.isoch_buffers_registered,
        })
        .unwrap_or_default()
}

fn sco_pcm_report(
    stats: sco::LinearPcmStats,
    selected_codec: Option<(String, u16)>,
) -> ScoPcmReport {
    let (codec, sample_rate) = selected_codec
        .map(|(codec, sample_rate)| (Some(codec), Some(sample_rate)))
        .unwrap_or_else(|| (Some("CVSD".to_string()), Some(8000)));

    ScoPcmReport {
        codec,
        sample_rate,
        frames: stats.frames,
        samples: stats.samples,
        bad_frames: stats.bad_frames,
        min_sample: stats.min_sample,
        max_sample: stats.max_sample,
        last_frame_samples: stats.last_frame_samples,
    }
}

pub(crate) fn is_timeout_error(error: &str) -> bool {
    // Win32: ERROR_SEM_TIMEOUT = 121, raised by WinUSB when a pipe
    // read exceeds the configured pipe timeout. Linux/libusb: rusb
    // surfaces Error::Timeout whose Display impl formats as "libusb
    // timeout" via our wrapper in aokie_radio::libusb.
    error.contains("WinUSB error 121")
        || error.contains("Win32 error 121")
        || error.contains("libusb timeout")
}

fn sco_tx_silence_enabled() -> bool {
    std::env::var_os(AOKIE_SCO_WRITE_SILENCE_ENV).is_some()
}

fn sco_tx_tone_enabled() -> bool {
    std::env::var_os(AOKIE_SCO_WRITE_TONE_ENV).is_some()
}

fn fill_sco_tx_test_tone(
    queue: &mut sco::LinearPcmTxQueue,
    phase_samples: &mut usize,
    samples: usize,
) -> usize {
    if samples == 0 {
        return 0;
    }

    let tone = (0..samples)
        .map(|_| {
            let sample = square_tone_sample(*phase_samples);
            *phase_samples = (*phase_samples).wrapping_add(1);
            sample
        })
        .collect::<Vec<_>>();
    queue.push_samples(&tone)
}

fn square_tone_sample(phase_samples: usize) -> i16 {
    let half_period = (AOKIE_SCO_TX_TONE_SAMPLE_RATE / AOKIE_SCO_TX_TONE_HZ / 2).max(1);
    if (phase_samples / half_period) % 2 == 0 {
        AOKIE_SCO_TX_TONE_AMPLITUDE
    } else {
        -AOKIE_SCO_TX_TONE_AMPLITUDE
    }
}

fn hfp_auto_answer_enabled() -> bool {
    std::env::var_os(AOKIE_HFP_AUTO_ANSWER_ENV).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_defaults_match_current_btstack_setup() {
        assert_eq!(AOKIE_CLASSIC_NAME, "Aokie AI Assistant");
        assert_eq!(AOKIE_CLASS_OF_DEVICE, 0x200408);
        assert_eq!(AOKIE_EIR_UUIDS, &[0x111e, 0x1133, 0x1200]);
        assert_eq!(AOKIE_PAGE_TIMEOUT, 0x6000);
        assert_eq!(AOKIE_LINK_POLICY, 0x0005);
        assert_eq!(AOKIE_VOICE_SETTING, 0x0060);
        assert_eq!(AOKIE_SCAN_ENABLE_CONNECTABLE_DISCOVERABLE, 0x03);
        // AOK-BT-001: at-rest = page scan on (0x02, connectable), inquiry scan off.
        assert_eq!(AOKIE_SCAN_ENABLE_CONNECTABLE, 0x02);
        assert_eq!(AOKIE_LEGACY_PIN, "0000");
        assert_eq!(AOKIE_SCO_TX_QUEUE_SAMPLES, 480_000);
        assert_eq!(AOKIE_SCO_USB_PAYLOAD_BYTES, 24);
        assert_eq!(AOKIE_SCO_WRITE_SILENCE_ENV, "AOKIE_RADIO_SCO_TX_SILENCE");
        assert_eq!(AOKIE_SCO_WRITE_TONE_ENV, "AOKIE_RADIO_SCO_TX_TONE");
        assert_eq!(AOKIE_SCO_TX_TONE_SAMPLE_RATE, 8_000);
        assert_eq!(AOKIE_SCO_TX_TONE_HZ, 440);
        assert_eq!(AOKIE_SCO_TX_TONE_AMPLITUDE, 4_000);
        assert_eq!(AOKIE_HFP_AUTO_ANSWER_ENV, "AOKIE_RADIO_AUTO_ANSWER");
    }

    #[test]
    fn pairing_admission_gate_bonds_only_in_window_but_reconnects_always() {
        // AOK-BT-001 acceptance: an UNKNOWN device can only connect / bond while a
        // pairing window is open; a BONDED device always reconnects; a fresh bond
        // is never completed at rest.

        // ACL connection admission.
        assert!(
            may_accept_acl_connection(true, false),
            "bonded device reconnects at rest"
        );
        assert!(
            may_accept_acl_connection(true, true),
            "bonded device connects during a window too"
        );
        assert!(
            may_accept_acl_connection(false, true),
            "unknown device connects inside the pairing window"
        );
        assert!(
            !may_accept_acl_connection(false, false),
            "unknown device is REFUSED at rest"
        );

        // Fresh-bond completion (PIN / SSP replies + link-key store).
        assert!(
            may_complete_new_bond(true),
            "new bond allowed in the window"
        );
        assert!(
            !may_complete_new_bond(false),
            "no new bond can complete at rest, even for a device mid-handshake"
        );
    }

    #[test]
    fn pairing_policy_postures_match_pair_001() {
        // PAIR-001 acceptance: production holds SSP for the operator (numeric
        // comparison) and refuses the fixed legacy PIN unless the compat
        // setting is explicitly on; diagnostic CLIs stay just-works.
        let diag = PairingPolicy::diagnostic();
        assert!(diag.pairing_open, "a diag session IS the operator action");
        assert!(diag.legacy_pin_allowed, "bench dongles keep the PIN path");
        assert!(!diag.operator_confirm, "diag auto-confirms just-works");

        let at_rest = PairingPolicy::runtime(false, false);
        assert!(!at_rest.pairing_open);
        assert!(!at_rest.legacy_pin_allowed, "legacy PIN is OFF by default");
        assert!(at_rest.operator_confirm, "production always holds SSP");

        let compat = PairingPolicy::runtime(true, true);
        assert!(compat.pairing_open && compat.legacy_pin_allowed);
        assert!(
            compat.operator_confirm,
            "the compat setting must not disable numeric comparison for SSP peers"
        );
    }

    #[test]
    fn reject_connection_command_is_well_formed() {
        let cmd = hci::reject_connection_request_command("00:19:86:00:22:6C").unwrap();
        assert_eq!(
            &cmd[0..2],
            &hci::OPCODE_REJECT_CONNECTION_REQUEST.to_le_bytes()
        );
        assert_eq!(cmd[2], 7, "param length = 6 addr + 1 reason");
        assert_eq!(cmd[9], 0x0F, "reason = Unacceptable BD_ADDR");
    }

    #[test]
    fn event_record_preserves_auto_action() {
        let record = event_record(
            hci::HciEvent::LinkKeyRequest {
                address: "00:19:86:00:22:6C".to_string(),
            },
            Some("sent negative link-key reply".to_string()),
        );
        assert_eq!(record.event_code, hci::EVENT_LINK_KEY_REQUEST);
        assert_eq!(record.name, "Link Key Request");
        assert_eq!(record.summary, "00:19:86:00:22:6C");
        assert_eq!(
            record.action.as_deref(),
            Some("sent negative link-key reply")
        );
    }

    #[test]
    fn hfp_event_record_formats_app_relevant_events() {
        assert_eq!(
            hfp_event_record(hfp::HfpEvent::CallerId("+15551234567".to_string())),
            HfpEventRecord {
                name: "CallerId".to_string(),
                summary: "+15551234567".to_string(),
            }
        );
        assert_eq!(
            hfp_event_record(hfp::HfpEvent::CodecSelected {
                codec: "mSBC".to_string(),
                sample_rate: 16000,
            }),
            HfpEventRecord {
                name: "CodecSelected".to_string(),
                summary: "mSBC:16000".to_string(),
            }
        );
    }

    #[test]
    fn acl_packet_read_len_floors_at_4kb_for_large_inbound_packets() {
        // Typical controllers advertise ~1021 for host→controller, but
        // controller→host can deliver larger packets (Bluedroid sends
        // up to a few KB on bMessage GET responses). Floor the read
        // buffer at 4 KB so we don't truncate inbound ACL mid-packet
        // and desync the parser.
        let buffer = hci::BufferSize {
            acl_data_packet_length: 1021,
            sco_data_packet_length: 60,
            total_num_acl_data_packets: 8,
            total_num_sco_data_packets: 4,
        };
        assert_eq!(max_acl_packet_len(&buffer), 4096);
        assert_eq!(max_sco_packet_len(&buffer), 63);
        assert_eq!(max_sco_payload_len(&buffer), 60);

        // A controller that actually advertises something bigger than
        // the floor wins — we don't clamp downward, only upward.
        let big = hci::BufferSize {
            acl_data_packet_length: 8192,
            ..buffer
        };
        assert_eq!(max_acl_packet_len(&big), 4 + 8192);

        let unknown = hci::BufferSize {
            sco_data_packet_length: 0,
            ..buffer
        };
        assert_eq!(max_sco_packet_len(&unknown), 255);
        assert_eq!(max_sco_payload_len(&unknown), 252);
    }

    #[test]
    fn sco_audio_report_preserves_stats() {
        let mut stats = sco::ScoStats::default();
        stats
            .record_packet(&sco::build_sco_packet(0x000b, 1, &[1, 2, 3]).unwrap())
            .unwrap();
        let report = sco_audio_report(stats, Some(0x000b));
        assert_eq!(report.packets, 1);
        assert_eq!(report.payload_bytes, 3);
        assert_eq!(report.active_connection_handle, Some(0x000b));
        assert_eq!(report.last_connection_handle, Some(0x000b));
        assert_eq!(report.last_packet_status_flag, Some(1));
    }

    #[test]
    fn sco_transport_report_preserves_usb_audio_config() {
        let report = sco_transport_report(Some(winusb::ScoTransportConfig {
            alternate_setting: 2,
            in_pipe_id: Some(0x83),
            out_pipe_id: Some(0x03),
            max_packet_size: Some(49),
            isoch_buffer_len: Some(441),
            isoch_buffers_registered: true,
        }));
        assert_eq!(report.alternate_setting, Some(2));
        assert_eq!(report.in_pipe_id, Some(0x83));
        assert_eq!(report.out_pipe_id, Some(0x03));
        assert_eq!(report.max_packet_size, Some(49));
        assert_eq!(report.isoch_buffer_len, Some(441));
        assert!(report.isoch_buffers_registered);
        assert_eq!(sco_transport_report(None), ScoTransportReport::default());
    }

    #[test]
    fn sco_tx_test_tone_fills_linear_pcm_queue() {
        let mut queue = sco::LinearPcmTxQueue::new(32);
        let mut phase = 0;

        assert_eq!(fill_sco_tx_test_tone(&mut queue, &mut phase, 20), 20);
        assert_eq!(queue.len(), 20);
        assert_eq!(phase, 20);

        let packet = queue.pop_sco_packet(0x000b, 40).unwrap();
        let samples = sco::linear_pcm_samples_from_sco_packet(&packet).unwrap();
        assert_eq!(&samples[..9], &[AOKIE_SCO_TX_TONE_AMPLITUDE; 9]);
        assert_eq!(&samples[9..18], &[-AOKIE_SCO_TX_TONE_AMPLITUDE; 9]);
    }

    #[test]
    fn sco_accept_parameters_pick_msbc_t2_when_codec_is_msbc() {
        let codec = ("mSBC".to_string(), 16000);
        let params = sco_accept_parameters(hci::LINK_TYPE_ESCO, Some(&codec));
        assert_eq!(params.voice_setting, AOKIE_VOICE_SETTING_TRANSPARENT);
        assert_eq!(params.max_latency, HFP_T2_MAX_LATENCY_MS);
        assert_eq!(params.retransmission_effort, HFP_T2_RETRANSMISSION_EFFORT);
        assert_eq!(
            params.packet_types,
            hci::SCO_PACKET_TYPES_HFP_CVSD_ESCO_COMMAND
        );
        assert_eq!(params.codec_label, "mSBC/T2");
    }

    /// R6/P1-5 regression. The mSBC transparent path MUST keep
    /// bit 5 of `voice_setting` clear (0x0043, not 0x0063). The
    /// 0x0063 variant (bit 5 set = "16-bit input sample size")
    /// makes the controller treat our pre-encoded mSBC byte stream
    /// as little-endian 16-bit samples and silently corrupts every
    /// odd byte — the air goes silent, RX side has no idea why.
    /// This was R5-#WBS / commit bf95183 to resolve and the doc-
    /// drift caught in R6 nearly walked us back into it.
    #[test]
    fn msbc_voice_setting_keeps_bit5_clear() {
        // The constant itself.
        assert_eq!(
            AOKIE_VOICE_SETTING_TRANSPARENT, 0x0043,
            "mSBC voice setting must be 0x0043 (transparent + 8-bit input)"
        );
        assert_eq!(
            AOKIE_VOICE_SETTING_TRANSPARENT & 0x0020,
            0,
            "bit 5 (sample size) MUST be clear — setting it (0x0063) is the \
             silent-mSBC-TX failure mode documented at the constant's site"
        );
        // And the path that produces it on a live mSBC eSCO accept.
        let codec = ("mSBC".to_string(), 16000);
        let params = sco_accept_parameters(hci::LINK_TYPE_ESCO, Some(&codec));
        assert_eq!(
            params.voice_setting & 0x0020,
            0,
            "sco_accept_parameters must keep bit 5 clear for mSBC; got {:#06x}",
            params.voice_setting
        );
    }

    #[test]
    fn sco_accept_parameters_fall_back_to_cvsd_when_codec_unknown() {
        let params = sco_accept_parameters(hci::LINK_TYPE_ESCO, None);
        assert_eq!(params.voice_setting, AOKIE_VOICE_SETTING);
        assert_eq!(params.max_latency, 0xffff);
        assert_eq!(
            params.retransmission_effort,
            hci::SCO_RETRANSMISSION_EFFORT_DONT_CARE
        );
        assert_eq!(params.codec_label, "CVSD");
    }

    #[test]
    fn sco_accept_parameters_force_cvsd_for_legacy_sco_link_even_with_msbc() {
        // The AG should never request a legacy HV link if it picked mSBC,
        // but if it does we don't want to send T2 params over an SCO that
        // can't carry mSBC frames — fall back to CVSD HV.
        let codec = ("mSBC".to_string(), 16000);
        let params = sco_accept_parameters(hci::LINK_TYPE_SCO, Some(&codec));
        assert_eq!(params.voice_setting, AOKIE_VOICE_SETTING);
        assert_eq!(
            params.packet_types,
            hci::SCO_PACKET_TYPES_HFP_CVSD_SCO_COMMAND
        );
    }

    #[test]
    fn selected_codec_tracks_hfp_codec_events() {
        let mut selected = None;
        update_selected_codec(&mut selected, &hfp::HfpEvent::IncomingCall);
        assert_eq!(selected, None);
        update_selected_codec(
            &mut selected,
            &hfp::HfpEvent::CodecSelected {
                codec: "mSBC".to_string(),
                sample_rate: 16000,
            },
        );
        assert_eq!(selected, Some(("mSBC".to_string(), 16000)));
    }

    #[test]
    fn hfp_auto_answer_reports_missing_open_channel() {
        let mut state = l2cap::L2capState::new();
        let mut report = HfpControlReport {
            auto_answer_enabled: true,
            ..Default::default()
        };
        let mut answer_sent = false;
        let packets = hfp_call_control_packets_for_event(
            &mut state,
            &hfp::HfpEvent::IncomingCall,
            &mut answer_sent,
            &mut report,
        )
        .unwrap();

        assert!(packets.is_empty());
        assert!(!answer_sent);
        assert_eq!(report.answer_attempts, 1);
        assert_eq!(report.answer_packets_sent, 0);
        assert_eq!(
            report.last_action.as_deref(),
            Some("could not auto-answer: no open HFP channel")
        );
    }

    /// Phase 4: a waiting knock triggers the AT+CLCC topology query (like
    /// the CallAnswered caller-id rescue) — and NEVER an answer attempt,
    /// even with auto-answer armed: plain ATA cannot answer a waiting call.
    #[test]
    fn call_waiting_triggers_clcc_topology_query_never_an_answer() {
        let mut state = l2cap::L2capState::new();
        let mut report = HfpControlReport {
            auto_answer_enabled: true,
            ..Default::default()
        };
        let mut answer_sent = true; // the ACTIVE call was answered earlier
        let packets = hfp_call_control_packets_for_event(
            &mut state,
            &hfp::HfpEvent::CallWaiting(Some("0491570157".to_string())),
            &mut answer_sent,
            &mut report,
        )
        .unwrap();
        // No open channel in this rig, so no packets — but the arm ran (no
        // answer attempt was made and the flag is untouched).
        assert!(packets.is_empty());
        assert!(answer_sent, "a knock must never reset the answered flag");
        assert_eq!(report.answer_attempts, 0, "never ATA a waiting call");
    }

    #[test]
    fn hfp_auto_answer_ignores_when_disabled_and_resets_on_termination() {
        let mut state = l2cap::L2capState::new();
        let mut report = HfpControlReport::default();
        let mut answer_sent = true;

        let packets = hfp_call_control_packets_for_event(
            &mut state,
            &hfp::HfpEvent::Ringing,
            &mut answer_sent,
            &mut report,
        )
        .unwrap();
        assert!(packets.is_empty());
        assert!(answer_sent);
        assert_eq!(report.answer_attempts, 0);

        let _ = hfp_call_control_packets_for_event(
            &mut state,
            &hfp::HfpEvent::CallTerminated,
            &mut answer_sent,
            &mut report,
        )
        .unwrap();
        assert!(!answer_sent);
    }

    #[test]
    fn sco_pcm_report_defaults_to_cvsd_when_codec_unknown() {
        let stats = sco::LinearPcmStats {
            frames: 2,
            samples: 4,
            min_sample: Some(-10),
            max_sample: Some(20),
            last_frame_samples: Some(2),
            ..Default::default()
        };
        let report = sco_pcm_report(stats, None);
        assert_eq!(report.codec.as_deref(), Some("CVSD"));
        assert_eq!(report.sample_rate, Some(8000));
        assert_eq!(report.frames, 2);
        assert_eq!(report.samples, 4);
        assert_eq!(report.min_sample, Some(-10));
        assert_eq!(report.max_sample, Some(20));
        assert_eq!(report.last_frame_samples, Some(2));

        let report = sco_pcm_report(Default::default(), Some(("mSBC".to_string(), 16000)));
        assert_eq!(report.codec.as_deref(), Some("mSBC"));
        assert_eq!(report.sample_rate, Some(16000));
    }

    #[test]
    fn timeout_detection_matches_winusb_timeout_errors() {
        assert!(is_timeout_error("WinUSB error 121"));
        assert!(is_timeout_error("Win32 error 121"));
        assert!(is_timeout_error(
            "WinUSB error 121: HCI SCO isoch read timed out"
        ));
        assert!(!is_timeout_error("Win32 error 5"));
    }
}
