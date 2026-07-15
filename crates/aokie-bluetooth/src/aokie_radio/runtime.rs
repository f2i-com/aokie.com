//! Long-lived runtime that drives a real phone call over the Aokie radio
//! stack. This is the production sibling of
//! `manager::listen_runtime_controller_with_options` (which is the diag
//! variant — runs for a fixed duration and produces a report).
//!
//! The runtime owns the HCI transport on a dedicated OS thread and
//! exposes:
//!  - `event_rx` / `audio_rx`: tokio channels the Tauri side polls each
//!    tick (the existing `BluetoothManager::try_recv_*` shape).
//!  - `control_tx` (private): commands flowing in — Answer / Hangup /
//!    SendAudio / Shutdown.
//!  - `status`: cheap atomic snapshot for `is_initialized`,
//!    `is_connected`, `sample_rate`, `local_address`, etc.
//!
//! Codec routing: when the AG selects mSBC, SCO RX runs every 60-byte
//! HCI SCO packet through `crate::msbc::H2Decoder` to recover 16 kHz PCM,
//! and SCO TX drains 120-sample blocks from `sco_tx_queue` through
//! `crate::msbc::H2Encoder` to build 60-byte H2-framed packets paced at
//! 7.5 ms each. The CVSD path is unchanged: linear-PCM passthrough at
//! 8 kHz with 48-byte HCI payloads paced at 3 ms each.

use crate::aokie_radio::bmessage;
use crate::aokie_radio::hfp::{ChldAction, HfpAtCommand, HfpEvent};
use crate::aokie_radio::manager::{
    self, AOKIE_SCO_TX_QUEUE_SAMPLES, AOKIE_SCO_USB_PAYLOAD_BYTES, AOKIE_VOICE_SETTING,
    AOKIE_VOICE_SETTING_TRANSPARENT,
};
use crate::aokie_radio::map_listing::parse_listing;
use crate::aokie_radio::map_mas::{
    Folder as MasFolder, Operation as MasOperation, CHARSET_UTF8, MSG_TYPE_KEEP_BOTH_SMS_VARIANTS,
};
use crate::aokie_radio::map_mns::{MnsEvent, MnsServer, MnsState};
use crate::aokie_radio::map_runtime::{MapRuntime, MapRuntimeEvent};
use crate::aokie_radio::pairing_store::AokiePairingStore;
use crate::aokie_radio::hfp_connect::{HfpConnectEvent, HfpConnectRuntime};
use crate::aokie_radio::pbap_runtime::{PbapRuntime, PbapRuntimeEvent};
use crate::aokie_radio::rfcomm::{
    build_modem_status_command, build_ua, build_uih, server_channel_dlci, RfcommState,
    ServerChannelHandlers, RFCOMM_DLCI_MULTIPLEXER, RFCOMM_LOCAL_MODEM_STATUS,
};
use crate::aokie_radio::sdp::AOKIE_MNS_RFCOMM_CHANNEL;
use crate::aokie_radio::transport::{self as winusb, AokieHciTransport};
use crate::aokie_radio::{hci, l2cap, sco, sco_dump};
use crate::msbc::{H2Decoder, MsbcStreamFramer, MsbcStreamPackager, MSBC_SAMPLES_PER_FRAME};

/// Audio bytes per millisecond on an mSBC SCO link: 60-byte H2 frames
/// every 7.5 ms = 8 bytes/ms. Drives the TX pacer's interval — at this
/// rate, an N-byte HCI SCO packet covers `N / 8` ms of audio.
const MSBC_AUDIO_BYTES_PER_MS: u128 = 8;
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{mpsc as stdmpsc, Arc, Mutex as StdMutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender};

/// Events the Tauri side cares about. Mirrors the shape of
/// `crate::bluetooth::BluetoothEvent` so the BTstack→aokie swap is a
/// straight forward translation in the bluetooth manager wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    Initialized(String),
    DeviceConnected(String),
    DeviceDisconnected(String),
    CallIncoming,
    CallRinging,
    /// Phase 2: an OUTBOUND call setup started (`callsetup,2`) — the plugin
    /// dialed via ATD, or the phone's owner dialed on the handset. The
    /// eventual `CallAnswered` is the REMOTE party picking up.
    OutgoingDialing,
    CallAnswered,
    CallTerminated,
    AudioConnected {
        codec: String,
        sample_rate: u16,
        /// Whether the USB iso pipes actually ARMED (audit AOK-HW-001):
        /// false = the SCO link is up at HCI but the alternate-setting
        /// configure failed — audio WILL be silent this call. The link is
        /// kept (tearing down would drop HFP/MAP/PBAP too) but readiness
        /// surfaces must never show a silent path as fully ready.
        armed: bool,
    },
    AudioDisconnected,
    CallerId(String),
    /// Phase 4: a SECOND caller is knocking while a call is active (+CCWA /
    /// callsetup-while-active on a call-waiting-negotiated SLC). One per
    /// waiting episode, plus one upgrade when a number arrives late.
    CallWaiting { number: Option<String> },
    /// The waiting episode ended with the active call untouched.
    CallWaitingEnded,
    /// `callheld` indicator transition (0 none / 1 held+active / 2 held only).
    CallHeld { state: i32 },
    /// One parsed `+CLCC:` line (Phase 4 observe topology): `status` 0
    /// active / 1 held / 2 dialing / 3 alerting / 4 incoming / 5 waiting.
    /// A CLCC response bursts one of these per current call.
    CallListEntry {
        index: u8,
        direction: u8,
        status: u8,
        multiparty: bool,
        number: Option<String>,
    },
    /// Phase 3e: phonebook fetch finished. The runtime IO loop
    /// produces this once per ACL connection after PBAP completes.
    /// One entry per (phone, display name) pair — vCards with
    /// multiple phone numbers expand into multiple rows here so the
    /// lookup table can be a flat HashMap.
    PbapContactsFetched(Vec<RuntimeContact>),
    /// Phase 4e: NotificationRegistration was acked by the AG, so
    /// the phone is now subscribed to push EventReports to our MNS
    /// server. Diagnostic-only; the UI doesn't act on it.
    MapNotificationsSubscribed,
    /// Phase 4e: a new SMS arrived. The runtime received an
    /// EventReport from the phone, fetched the bMessage via MAP MAS,
    /// and parsed it. `sender_phone` is empty when the AG omitted the
    /// originator vCard's TEL field (some phones do this).
    SmsReceived {
        sender_phone: String,
        sender_name: Option<String>,
        body: String,
        handle: String,
        msg_type: Option<String>,
    },
    /// Phase 4e: an outbound SMS PUT we initiated via `send_sms` was
    /// acknowledged by the AG. The phone has accepted the bMessage —
    /// it'll forward to the carrier on its own schedule.
    SmsSent {
        recipient_phone: String,
    },
    /// An outbound SMS was ABANDONED — the MAS session failed the PUT,
    /// or the queued op aged past its retain TTL across recovery
    /// cycles. Emitted so the message is never lost silently (audit
    /// C-16: a caller was once promised an SMS that never existed).
    SmsSendFailed {
        recipient_phone: String,
        reason: String,
    },
    /// PAIR-001: SSP numeric comparison is HELD for the operator — the
    /// phone shows the same `numeric_value`; the operator answers via
    /// `confirm_pairing(address, accept)`. Expires with a negative reply
    /// after `PAIRING_CONFIRM_TIMEOUT_SECS`.
    PairingConfirmRequired {
        address: String,
        numeric_value: u32,
    },
    Error(String),
}

/// PAIR-001: how long a held SSP numeric confirmation waits for the
/// operator before the runtime answers negatively on their behalf. The
/// LMP response timeout aborts the handshake at ~30 s regardless, so
/// waiting longer would only surface a stale prompt.
pub const PAIRING_CONFIRM_TIMEOUT_SECS: u64 = 25;

/// PAIR-001: the held SSP numeric comparison awaiting operator input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPairingConfirm {
    pub address: String,
    pub numeric_value: u32,
    /// Unix-epoch second past which the runtime auto-refuses.
    pub expires_epoch_secs: u64,
}

/// PAIR-001: shared, lock-cheap view of the pending confirmation so
/// `phone.status` can report it without an RPC to the runtime thread —
/// same shape as `PairingWindow`.
#[derive(Clone, Default)]
pub struct PairingConfirmSlot {
    inner: Arc<RwLock<Option<PendingPairingConfirm>>>,
}

impl PairingConfirmSlot {
    pub fn set(&self, pending: PendingPairingConfirm) {
        if let Ok(mut slot) = self.inner.write() {
            *slot = Some(pending);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut slot) = self.inner.write() {
            *slot = None;
        }
    }

    /// The pending confirmation, if any — expired entries read as None
    /// (the runtime loop sends the negative reply on its next tick).
    pub fn get(&self) -> Option<PendingPairingConfirm> {
        let pending = self.inner.read().ok().and_then(|slot| slot.clone())?;
        if pending.expires_epoch_secs <= now_epoch_secs() {
            return None;
        }
        Some(pending)
    }
}

/// Phase 3e contact tuple shipped from the runtime thread to the
/// Tauri layer. Kept simple so it can cross thread boundaries via
/// `mpsc` without `Arc<>` overhead. Persistence + normalization
/// happens on the Tauri side using `crate::database::normalize_number`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContact {
    pub phone_number: String,
    pub display_name: String,
}

/// Decoded SCO audio frame heading toward the Tauri pipeline.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub samples: Vec<i16>,
    pub sample_rate: u16,
}

enum ControlCommand {
    Answer,
    RejectOrHangup,
    /// Phase 2: place an outbound voice call (`ATD<number>;`). Call-state
    /// truth stays with the AG's +CIEV stream — the runtime only puts the
    /// command on the wire; dialing/alerting/answer events follow from the
    /// indicator updates exactly like an incoming call's do.
    Dial(String),
    /// Phase 4 (switchboard): `AT+CHLD=2` — hold the active call and
    /// accept the waiting/held one. The ONLY non-destructive switch on
    /// phones without indexed CHLD modes (the live Pixel advertises
    /// (0,1,2,3) only). TOGGLE semantics: never blind-retry after a lost
    /// OK — the caller reconciles with a fresh AT+CLCC instead. State
    /// truth stays with the AG's indicator stream + CLCC, exactly like
    /// Answer/Dial.
    HoldSwap,
    SendAudio(Vec<i16>),
    /// Drain `sco_tx_queue` immediately, dropping any TTS bytes that were
    /// already queued for transmission. Used by "Take Over Call" so the
    /// bot stops mid-syllable instead of finishing the in-flight sentence
    /// already buffered in the SCO TX ring.
    FlushTxAudio,
    /// Phase 4e: send an SMS via MAP MAS PushMessage. Build a
    /// bMessage envelope around `body` with `recipient_phone` as the
    /// recipient TEL, push through a `MapRuntime`, and emit
    /// `SmsSent` once the AG acks the PUT.
    ///
    /// `msg_type` echoes the inbound type so the auto-reply lands in
    /// the same thread on the sender's phone — `Some("MMS")` selects
    /// the MMS bMessage builder (with a MIME multipart body so
    /// Pixel's MMS dispatcher routes via the same store the inbound
    /// came from, including its RCS-upgrade path), anything else
    /// (including None for manual sends) falls back to plain
    /// SMS_GSM.
    SendSms {
        recipient_phone: String,
        body: String,
        msg_type: Option<String>,
    },
    /// AOK-BT-001: forget a bonded device. The pairing store lives on the runtime
    /// thread, so removal is an RPC — the loop drops the link key, refreshes the
    /// bonded snapshot, and replies over `reply` with whether a record was removed.
    RemovePaired {
        address: String,
        reply: stdmpsc::Sender<Result<bool, String>>,
    },
    /// Disconnect the active ACL for `address` but KEEP the bond (unlike
    /// RemovePaired). Clears a wedged link; the phone re-pages us and
    /// reconnects (working inbound direction). Replies whether a live link
    /// for that address was actually disconnected.
    Disconnect {
        address: String,
        reply: stdmpsc::Sender<Result<bool, String>>,
    },
    /// HARD-001: reconnect a BONDED phone from our side — page it, then
    /// drive SDP → RFCOMM → HFP SLC ourselves (Bluedroid initiates no
    /// profiles when it was the paged side; see `hfp_connect.rs`).
    /// Replies `Ok(true)` when the page was started (the authoritative
    /// outcome is the phone.connected event once the SLC lands),
    /// `Ok(false)` when that phone is already connected, `Err` when the
    /// address isn't bonded, another phone holds the link, or the HCI
    /// write failed.
    Connect {
        address: String,
        reply: stdmpsc::Sender<Result<bool, String>>,
    },
    /// PAIR-001: resolve a held SSP numeric confirmation. `accept` sends the
    /// positive reply (bond proceeds); `false` sends the negative reply. `Err`
    /// when nothing is pending for `address` (expired / wrong address).
    ConfirmPairing {
        address: String,
        accept: bool,
        reply: stdmpsc::Sender<Result<(), String>>,
    },
    Shutdown,
}

/// A bounded Bluetooth pairing window (audit AOK-BT-001). While open, the radio
/// is discoverable and will bond a NEW device; at rest it is connectable-only, so
/// only already-bonded phones can reconnect and strangers can neither discover nor
/// pair. Backed by a shared `Arc<AtomicU64>` holding the window deadline as a
/// Unix-epoch second (0 = closed), so the control thread (which opens/closes it)
/// and the runtime loop (which reconciles the controller's scan-enable and reads
/// the gate) share one lock-free source of truth.
#[derive(Clone, Default)]
pub struct PairingWindow {
    deadline_epoch_secs: Arc<AtomicU64>,
}

impl PairingWindow {
    /// Open (or extend) the window for `seconds` from now. Clamped to a minimum
    /// of 1 second so `open_for(0)` can't create an already-expired window.
    pub fn open_for(&self, seconds: u64) {
        let until = now_epoch_secs().saturating_add(seconds.max(1));
        self.deadline_epoch_secs.store(until, Ordering::SeqCst);
    }

    pub fn close(&self) {
        self.deadline_epoch_secs.store(0, Ordering::SeqCst);
    }

    /// Seconds remaining before the window closes; 0 when closed or expired.
    pub fn remaining_secs(&self) -> u64 {
        self.deadline_epoch_secs
            .load(Ordering::SeqCst)
            .saturating_sub(now_epoch_secs())
    }

    pub fn is_open(&self) -> bool {
        self.remaining_secs() > 0
    }
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Phase 4e: queued MAS operations that share a single MapRuntime
/// slot. We run them sequentially because each ops pulls and tears
/// down its own RFCOMM/SDP channels — running two in parallel would
/// race the L2CAP CID allocator and confuse the AG.
#[derive(Debug, Clone)]
enum PendingMapOp {
    /// Subscribe to MNS notifications. Sent once per ACL connection
    /// after SLC completes.
    Subscribe,
    /// Fetch the bMessage body for a handle the phone reported via
    /// MNS EventReport. `queued_at` lets the disconnect handler keep
    /// fresh fetches alive across stage-2 stall recoveries — handles
    /// are persistent on Pixel/Bluedroid, so the same GET works on
    /// the next MAS session.
    FetchMessage { handle: String, queued_at: Instant },
    /// Push a pre-built bMessage as an outbound SMS. The body is the
    /// full envelope (BEGIN:BMSG ... END:BMSG); see
    /// `bmessage::build_sms_push`.
    ///
    /// `queued_at` is used by the ACL-disconnect handler to decide
    /// whether to preserve this op across the link cycle: a SendReply
    /// queued seconds before a stage-2 stall recovery is still worth
    /// retrying once the new ACL comes up, but one queued half an hour
    /// ago is stale and should be dropped to avoid surprise late
    /// replies when the user reconnects.
    SendReply {
        bmessage: Vec<u8>,
        recipient_phone: String,
        queued_at: Instant,
    },
    /// Pull the inbox listing as a fallback for missed MNS pushes.
    /// Pixel intermittently swallows NewMessage notifications even when
    /// MNS RFCOMM stays open; without this poll, those messages would
    /// stay invisible until the next ACL re-pair.
    PollInbox,
}

impl PendingMapOp {
    /// Render a redacted, log-safe summary of the op. The raw `Debug`
    /// derive on `SendReply` would dump the full bMessage bytes plus
    /// the recipient phone — exactly the PII payload we want to keep
    /// out of default logs.
    fn log_summary(&self) -> String {
        match self {
            PendingMapOp::Subscribe => "Subscribe".to_string(),
            PendingMapOp::FetchMessage { handle, .. } => {
                format!("FetchMessage(handle={})", handle)
            }
            PendingMapOp::SendReply {
                bmessage,
                recipient_phone,
                ..
            } => {
                format!(
                    "SendReply(to={}, bmessage={}B)",
                    aokie_core::redact::Phone(recipient_phone),
                    bmessage.len()
                )
            }
            PendingMapOp::PollInbox => "PollInbox".to_string(),
        }
    }
}

#[derive(Default)]
pub struct RuntimeStatus {
    initialized: AtomicBool,
    connected: AtomicBool,
    call_active: AtomicBool,
    sample_rate: AtomicU16,
    addresses: RwLock<Addresses>,
    /// Cumulative count of audio frames the runtime had to drop because
    /// the bounded audio channel was full. Bumped each time a SCO RX
    /// path's `try_send` returns `Err(Full(_))` — usually a sign that the
    /// downstream consumer (Whisper inference, the bot turn loop)
    /// stalled long enough that 2-ish seconds of audio backed up. Read
    /// by callers via `dropped_audio_frames()` to surface it as a
    /// diagnostic gauge.
    audio_dropped: AtomicU64,
    /// Set true if the runtime thread did not exit within its shutdown
    /// deadline. Lets the UI surface "still draining USB transport" or
    /// equivalent instead of silently leaking a zombie thread. Stays
    /// `true` for the rest of process lifetime since by definition we
    /// detached the thread and can't observe whether it ever finished.
    shutdown_timed_out: AtomicBool,
    /// AOK-BT-001: the bounded pairing window. Opened by `open_pairing_window`
    /// (phone.startPairing), closed on `close_pairing_window`/timeout. The
    /// runtime loop reconciles the controller's scan-enable to `is_open()` and
    /// passes it to the HCI pairing gate.
    pairing_window: PairingWindow,
    /// AOK-BT-001: a snapshot of the pairing store's bonded devices (BD_ADDR +
    /// captured friendly name, never link keys), refreshed at startup and after
    /// every bond/removal/name-capture, so `phone.listPaired` can show revocable
    /// identities WITH device names without touching the runtime thread's store.
    bonded_devices: RwLock<Vec<super::pairing_store::PairedDeviceRecord>>,
    /// PAIR-001: the held SSP numeric comparison awaiting the operator, if any.
    /// Written by the runtime loop, read by `phone.status` via the slot clone.
    pairing_confirm: PairingConfirmSlot,
}

/// Bounded audio channel depth. Each `AudioFrame` carries one SCO
/// packet's worth of samples (≈7.5 ms wall-clock at the 8/16 kHz link
/// rates we negotiate), so 256 buffered frames is roughly two seconds
/// of latency before the runtime starts dropping. That's enough head-
/// room for a slow Whisper iteration but caps total queued memory at
/// ≈48 KB even in the pathological "consumer hung mid-call" case.
const AUDIO_CHANNEL_DEPTH: usize = 256;

#[derive(Default)]
struct Addresses {
    local: String,
    remote: String,
    /// The connected phone's friendly name/model, captured via HCI Remote Name
    /// Request after the ACL comes up. Empty until it lands (~1s after connect).
    remote_name: Option<String>,
}

pub struct AokieRuntime {
    handle: Option<JoinHandle<()>>,
    control_tx: stdmpsc::Sender<ControlCommand>,
    event_rx: UnboundedReceiver<RuntimeEvent>,
    audio_rx: Receiver<AudioFrame>,
    status: Arc<RuntimeStatus>,
}

impl AokieRuntime {
    /// Spawn the runtime thread, opening the first available HCI radio
    /// interface and running the call event loop until `shutdown()` is
    /// called or the runtime is dropped. Equivalent to
    /// `start_with_options(path, None)`.
    pub fn start(pairing_store_path: PathBuf) -> Result<Self, String> {
        Self::start_with_options(pairing_store_path, None)
    }

    /// Variant that lets the caller pin which dongle the runtime
    /// opens. `preferred_dongle_path = Some(p)` asks the runtime to
    /// match interface `p`; an unmatched preference falls back to
    /// enumeration-first with a log line so a moved-port dongle still
    /// works without the operator manually clearing their pick.
    pub fn start_with_options(
        pairing_store_path: PathBuf,
        preferred_dongle_path: Option<String>,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        // Audio channel is bounded — see AUDIO_CHANNEL_DEPTH for sizing
        // rationale. The send sites use `try_send` and increment
        // `status.audio_dropped` on overflow rather than blocking; the
        // BT runtime thread can't afford to back-pressure on SCO
        // packet handling without breaking real-time audio. Events
        // stay unbounded because they're low-volume (a handful per
        // call) and must arrive in order.
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_CHANNEL_DEPTH);
        let (control_tx, control_rx) = stdmpsc::channel();
        let status = Arc::new(RuntimeStatus::default());
        let status_thread = status.clone();
        let event_tx_thread = event_tx.clone();

        // 8 MB stack — generous for the deep ACL → L2CAP → RFCOMM →
        // HFP dispatch and the live state machines we keep on the
        // stack. The Windows default of 1 MB was too tight under a
        // live call (we hit STATUS_STACK_OVERFLOW right after
        // CallAnswered).
        let handle = thread::Builder::new()
            .name("aokie-radio".to_string())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                // Capture a real backtrace if anything panics on this
                // thread — without it Windows shows only "thread …
                // overflowed its stack" with no clue where.
                //
                // We use catch_unwind locally instead of std::panic::set_hook
                // because the hook is process-global. Each runtime restart
                // would pile another frame on the chain (or stomp the
                // previous chain depending on order), and other threads
                // would observe the radio thread's hook for their own
                // panics. catch_unwind is thread-local: we get the panic
                // payload, log it with a backtrace, and the global hook is
                // never touched.
                let preferred_for_thread = preferred_dongle_path.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_runtime(
                        pairing_store_path,
                        preferred_for_thread,
                        event_tx_thread.clone(),
                        audio_tx,
                        control_rx,
                        status_thread.clone(),
                    )
                }));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        eprintln!("[AokieRadio] runtime exited with error: {}", e);
                        let _ = event_tx_thread.send(RuntimeEvent::Error(e));
                    }
                    Err(payload) => {
                        let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                            (*s).to_string()
                        } else if let Some(s) = payload.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "<non-string panic payload>".to_string()
                        };
                        eprintln!("[AokieRadio] PANIC on aokie-radio thread: {}", msg);
                        eprintln!(
                            "[AokieRadio] backtrace:\n{}",
                            std::backtrace::Backtrace::force_capture()
                        );
                        let _ = event_tx_thread.send(RuntimeEvent::Error(format!(
                            "aokie-radio thread panicked: {}",
                            msg
                        )));
                    }
                }
                status_thread.initialized.store(false, Ordering::Relaxed);
                status_thread.connected.store(false, Ordering::Relaxed);
                status_thread.call_active.store(false, Ordering::Relaxed);
            })
            .map_err(|e| format!("failed to spawn aokie-radio thread: {}", e))?;

        Ok(Self {
            handle: Some(handle),
            control_tx,
            event_rx,
            audio_rx,
            status,
        })
    }

    pub fn try_recv_event(&mut self) -> Option<RuntimeEvent> {
        self.event_rx.try_recv().ok()
    }

    pub fn try_recv_audio(&mut self) -> Option<AudioFrame> {
        self.audio_rx.try_recv().ok()
    }

    /// Equivalent to BTstack's `btstack_bridge_answer_call`. Takes effect
    /// as soon as the runtime thread services its control channel.
    pub fn answer(&self) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::Answer)
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())
    }

    pub fn reject_or_hangup(&self) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::RejectOrHangup)
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())
    }

    /// Phase 2: place an OUTBOUND voice call. The number is sanitized to
    /// digits (+ optional leading `+`) at the AT layer. Progress arrives as
    /// the normal event stream: `OutgoingDialing` → `CallRinging` (remote
    /// alerting) → `CallAnswered` / `CallTerminated`.
    pub fn dial(&self, number: String) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::Dial(number))
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())
    }

    /// Phase 4 (switchboard): `AT+CHLD=2` — hold the active call, accept
    /// the waiting/held one (the plain toggle; see ControlCommand::HoldSwap
    /// for the never-blind-retry rule). Outcome arrives via the indicator
    /// stream (callheld/callsetup) + a follow-up AT+CLCC.
    pub fn hold_swap(&self) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::HoldSwap)
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())
    }

    /// Push linearly-encoded PCM samples for the runtime to send over
    /// SCO. Samples must already be at the negotiated sample rate
    /// (8 kHz for CVSD, 16 kHz for mSBC). The runtime queues them and
    /// drains the queue inside its event loop.
    pub fn send_audio(&self, samples: Vec<i16>) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::SendAudio(samples))
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())
    }

    /// Drop any buffered TTS audio in the SCO TX queue. The bot keeps
    /// talking until this drains — without it, a "Take Over Call" click
    /// only stops *future* TTS while ~200 ms of already-queued audio
    /// continues to play.
    pub fn flush_tx_audio(&self) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::FlushTxAudio)
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())
    }

    /// Phase 4e: queue an outbound SMS for delivery via MAP MAS
    /// PushMessage. Fires `RuntimeEvent::SmsSent` once the AG acks
    /// the PUT (or `RuntimeEvent::Error` on failure). The runtime
    /// builds the bMessage envelope so callers only supply the
    /// recipient phone and the plain-text body.
    ///
    /// `msg_type` should echo whatever the inbound `SmsReceived`
    /// reported — pass `Some("MMS")` to build an MMS bMessage so
    /// the reply lands in the same thread the user's RCS/MMS
    /// fallback came from. `None` (or any other value) builds a
    /// plain SMS_GSM bMessage as before.
    pub fn send_sms(
        &self,
        recipient_phone: String,
        body: String,
        msg_type: Option<String>,
    ) -> Result<(), String> {
        self.control_tx
            .send(ControlCommand::SendSms {
                recipient_phone,
                body,
                msg_type,
            })
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())
    }

    pub fn shutdown(&self) {
        let _ = self.control_tx.send(ControlCommand::Shutdown);
    }

    /// AOK-BT-001: open a bounded pairing window for `seconds`. The window is
    /// shared state, so this takes effect immediately; the runtime loop makes
    /// the radio discoverable on its next tick and closes it on expiry.
    pub fn open_pairing_window(&self, seconds: u64) {
        self.status.pairing_window.open_for(seconds);
    }

    /// AOK-BT-001: close the pairing window now (operator cancel / pairing done).
    pub fn close_pairing_window(&self) {
        self.status.pairing_window.close();
    }

    /// AOK-BT-001: seconds left in the pairing window, 0 when closed.
    pub fn pairing_window_remaining_secs(&self) -> u64 {
        self.status.pairing_window.remaining_secs()
    }

    /// AOK-BT-001: a clone of the shared pairing window (Arc-backed) so a caller
    /// can read `remaining_secs()`/`is_open()` lock-free, reflecting expiry and
    /// auto-close-on-bond without polling the runtime thread.
    pub fn pairing_window(&self) -> PairingWindow {
        self.status.pairing_window.clone()
    }

    /// AOK-BT-001: the bonded devices' BD_ADDRs (never link keys) — the revocable
    /// identities `phone.listPaired` surfaces.
    pub fn bonded_addresses(&self) -> Vec<String> {
        self.status
            .bonded_devices
            .read()
            .map(|d| d.iter().map(|r| r.address.clone()).collect())
            .unwrap_or_default()
    }

    /// The bonded devices with their captured friendly names (address + name)
    /// so `phone.listPaired` can disambiguate multiple phones by model.
    pub fn bonded_devices(&self) -> Vec<super::pairing_store::PairedDeviceRecord> {
        self.status
            .bonded_devices
            .read()
            .map(|d| d.clone())
            .unwrap_or_default()
    }

    /// The connected phone's captured friendly name, if known yet.
    pub fn connected_name(&self) -> Option<String> {
        self.status
            .addresses
            .read()
            .ok()
            .and_then(|a| a.remote_name.clone())
    }

    /// Disconnect the currently-connected phone (KEEP the bond, unlike Forget).
    /// A wedged HFP link is cleared this way; the phone — for which we stay
    /// connectable — typically re-pages us and re-establishes the link (the
    /// working inbound direction), so this doubles as a remote reconnect.
    /// Returns true when a live link for `address` was actually disconnected.
    pub fn disconnect(&self, address: String) -> Result<bool, String> {
        let (reply_tx, reply_rx) = stdmpsc::channel();
        self.control_tx
            .send(ControlCommand::Disconnect {
                address,
                reply: reply_tx,
            })
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())?;
        reply_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "aokie-radio runtime did not answer the disconnect request".to_string())?
    }

    /// HARD-001: reconnect a bonded phone from OUR side — page it, then drive
    /// the SDP/RFCOMM/HFP setup the phone won't initiate when paged. Returns
    /// `Ok(true)` when the page started (the phone.connected event is the
    /// authoritative outcome), `Ok(false)` when that phone is already
    /// connected, `Err` for not-bonded / link-busy / HCI failures.
    pub fn connect(&self, address: String) -> Result<bool, String> {
        let (reply_tx, reply_rx) = stdmpsc::channel();
        self.control_tx
            .send(ControlCommand::Connect {
                address,
                reply: reply_tx,
            })
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())?;
        reply_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "aokie-radio runtime did not answer the connect request".to_string())?
    }

    /// PAIR-001: a clone of the shared pending-confirmation slot so callers can
    /// poll `phone.status` without an RPC to the runtime thread.
    pub fn pairing_confirm_slot(&self) -> PairingConfirmSlot {
        self.status.pairing_confirm.clone()
    }

    /// PAIR-001: resolve the held SSP numeric confirmation for `address`.
    /// Blocks briefly on the runtime thread (which owns the HCI transport).
    pub fn confirm_pairing(&self, address: String, accept: bool) -> Result<(), String> {
        let (reply_tx, reply_rx) = stdmpsc::channel();
        self.control_tx
            .send(ControlCommand::ConfirmPairing {
                address,
                accept,
                reply: reply_tx,
            })
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())?;
        reply_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| {
                "aokie-radio runtime did not answer the confirmPairing request".to_string()
            })?
    }

    /// AOK-BT-001: forget a bonded device. Blocks briefly on the runtime thread
    /// (the store's owner) and returns whether a link key was removed.
    pub fn remove_paired(&self, address: String) -> Result<bool, String> {
        let (reply_tx, reply_rx) = stdmpsc::channel();
        self.control_tx
            .send(ControlCommand::RemovePaired {
                address,
                reply: reply_tx,
            })
            .map_err(|_| "aokie-radio runtime is no longer running".to_string())?;
        reply_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "aokie-radio runtime did not answer the removePaired request".to_string())?
    }

    pub fn is_initialized(&self) -> bool {
        self.status.initialized.load(Ordering::Relaxed)
    }

    pub fn is_connected(&self) -> bool {
        self.status.connected.load(Ordering::Relaxed)
    }

    pub fn is_call_active(&self) -> bool {
        self.status.call_active.load(Ordering::Relaxed)
    }

    pub fn sample_rate(&self) -> u16 {
        self.status.sample_rate.load(Ordering::Relaxed)
    }

    pub fn local_address(&self) -> String {
        self.status
            .addresses
            .read()
            .map(|a| a.local.clone())
            .unwrap_or_default()
    }

    pub fn remote_address(&self) -> String {
        self.status
            .addresses
            .read()
            .map(|a| a.remote.clone())
            .unwrap_or_default()
    }

    /// Cumulative count of inbound SCO audio frames the runtime had to
    /// drop because the bounded audio channel was full. A non-zero
    /// value here means the consumer (Whisper inference, the bot turn
    /// loop) couldn't keep up with real time; the value is monotonic
    /// across the lifetime of the runtime.
    pub fn dropped_audio_frames(&self) -> u64 {
        self.status.audio_dropped.load(Ordering::Relaxed)
    }

    /// True if the runtime thread missed its shutdown deadline and was
    /// detached. Surfaced to the UI so a wedged dongle doesn't look the
    /// same as a clean exit.
    pub fn shutdown_timed_out(&self) -> bool {
        self.status.shutdown_timed_out.load(Ordering::Relaxed)
    }
}

/// How long Drop waits for the runtime thread to exit cleanly before
/// detaching. The thread's transport reads use 50 ms timeouts, so on a
/// healthy dongle it should observe the Shutdown command within a few
/// timeout cycles. The previous 500 ms budget was too tight when the
/// USB driver was still draining a bulk transfer queue — bumped to 2 s
/// which still keeps the Tauri shutdown path responsive but gives a
/// slow driver a fair chance to drain.
const SHUTDOWN_DEADLINE: Duration = Duration::from_millis(2_000);

impl Drop for AokieRuntime {
    fn drop(&mut self) {
        // First: signal the worker. This breaks it out of any
        // command-wait early; it'll observe the flag at the next
        // transport read timeout.
        self.shutdown();
        // Mark connected=false immediately so any UI watcher knows the
        // device is going away even before the thread observes it.
        self.status.initialized.store(false, Ordering::Relaxed);
        self.status.connected.store(false, Ordering::Relaxed);
        self.status.call_active.store(false, Ordering::Relaxed);

        if let Some(handle) = self.handle.take() {
            let deadline = Instant::now() + SHUTDOWN_DEADLINE;
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(25));
            }
            if handle.is_finished() {
                if let Err(payload) = handle.join() {
                    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "<non-string panic payload>".to_string()
                    };
                    eprintln!("[AokieRadio] runtime thread joined with panic: {}", msg);
                }
            } else {
                self.status
                    .shutdown_timed_out
                    .store(true, Ordering::Relaxed);
                eprintln!(
                    "[AokieRadio] runtime thread did not exit within {}ms; detaching — \
                     a wedged USB transfer is likely holding the bulk read open. \
                     The OS will reap on process exit.",
                    SHUTDOWN_DEADLINE.as_millis()
                );
                #[cfg(debug_assertions)]
                {
                    eprintln!(
                        "[AokieRadio] (debug) consider unplugging the dongle if this \
                         persists, or attach a debugger to the aokie-radio thread"
                    );
                }
            }
        }
    }
}

/// Outcome of one drain attempt against the runtime's ACL byte-stream
/// accumulator (see `pop_acl_frame`). All side effects are applied
/// directly to the buffer / `partial_since` clock; non-Frame variants
/// only carry the diagnostic data the caller logs at the call site.
#[derive(Debug, PartialEq, Eq)]
enum AclPopOutcome {
    /// Buffer holds <4 bytes — no header to parse yet. `partial_since`
    /// was cleared so a slow-trickling fragment doesn't get falsely
    /// timed out as "stuck".
    NotReady,
    /// Header parses, but the payload hasn't fully arrived AND the
    /// partial-stall timer hasn't fired. `partial_since` was stamped
    /// (or kept) at the supplied `now` so the caller can detect a
    /// wedged accumulator after `ACL_PARTIAL_TIMEOUT`.
    Partial { declared: usize, had: usize },
    /// Header sanity-check fired (declared > 8 KB), but a plausible
    /// ACL header was found later in the buffer. The leading garbage
    /// has been drained; caller should `continue` so the next call
    /// re-parses the resynced buffer.
    Resynced {
        dropped_prefix: usize,
        declared: usize,
        buffer_len_before: usize,
        first32: Vec<u8>,
    },
    /// Buffer was flushed entirely — either no resync target was
    /// found behind a >8 KB header, or a partial frame stayed
    /// outstanding past `ACL_PARTIAL_TIMEOUT`. Caller logs and
    /// breaks out of the drain loop so the next read can resync.
    Flushed {
        reason: AclFlushReason,
        declared: usize,
        had: usize,
        first32: Vec<u8>,
    },
    /// One complete ACL packet drained. Caller dispatches it through
    /// L2CAP / RFCOMM / HFP / MAP / PBAP.
    Frame(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
enum AclFlushReason {
    /// Header declared more than `ACL_FRAME_SANITY_MAX` bytes and the
    /// resync scan found no plausible ACL header anywhere in the
    /// remaining buffer.
    NoResyncTarget,
    /// A partial frame stayed outstanding for the supplied duration.
    /// The "header" is almost certainly mid-payload bytes from a
    /// drift; flush and let the next read resync.
    PartialStall(Duration),
}

/// HCI's ACL header is a 16-bit length field — the spec ceiling is
/// 65 539 B (4 + 65 535). In our HFP / MAP / PBAP traffic every ACL
/// is well under 200 B (RFCOMM 127-byte max_frame_size) or under
/// 1 KB (direct-L2CAP OBEX MTU). 8 KB gives generous headroom while
/// catching mid-payload misalignment quickly: a parser drifting off
/// frame and reading mid-payload bytes as a length almost always
/// produces a declared length above this, which lets us flush
/// before a real incoming-call RING piles up behind a bogus header.
const ACL_FRAME_SANITY_MAX: usize = 8192;
/// When sanity-MAX fires, scan forward looking for an ACL header
/// whose declared length is at most this. The tighter bound reduces
/// false positives when scanning byte-by-byte across a random
/// RFCOMM payload — well above any L2CAP/OBEX MTU we negotiate.
const ACL_RESYNC_LEN_MAX: usize = 1024;
/// How long a partial frame can stay outstanding before we give up
/// and flush. Real HCI fragments complete within milliseconds; 3 s
/// of "still partial" with bytes continuing to flow means we drifted
/// off frame and the "header" is mid-payload bytes that will never
/// resolve. Without this bound a one-time misalignment wedges the
/// entire ACL stream forever.
const ACL_PARTIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Try to drain one ACL packet from the head of `buffer`. The runtime
/// owns the buffer + `partial_since` clock and calls this in a loop
/// until it returns `NotReady` / `Partial`. Logging lives at the call
/// site — this function is pure mutation of the two arguments so its
/// behaviour is deterministically testable.
///
/// `now` is supplied by the caller so unit tests can drive the
/// partial-stall path without touching the real monotonic clock.
fn pop_acl_frame(
    buffer: &mut Vec<u8>,
    partial_since: &mut Option<Instant>,
    now: Instant,
    expected_handle: Option<u16>,
) -> AclPopOutcome {
    if buffer.len() < 4 {
        *partial_since = None;
        return AclPopOutcome::NotReady;
    }
    let header_handle_flags = u16::from_le_bytes([buffer[0], buffer[1]]);
    let header_handle = header_handle_flags & 0x0fff;
    let header_bc = (header_handle_flags >> 14) & 0x03;
    let payload_len = u16::from_le_bytes([buffer[2], buffer[3]]) as usize;
    let total_len = 4 + payload_len;
    // Treat the offset-0 header as corrupt if any of:
    //   * declared total exceeds the sanity ceiling (original heuristic)
    //   * BC bits are non-zero (we don't accept broadcast on this link)
    //   * an active ACL handle is known and doesn't match this header's
    //     handle (a flaky USB ring on some Broadcom dongles produces
    //     plausible-but-wrong ACL headers with random handles like
    //     0x006 / 0x535 — the loose-criteria resync accepts them and
    //     stalls 3s on a partial frame that will never complete; the
    //     active-handle check is a far stronger filter when we know it)
    let header_handle_mismatch = expected_handle.map(|h| header_handle != h).unwrap_or(false);
    let header_corrupt =
        total_len > ACL_FRAME_SANITY_MAX || header_bc != 0 || header_handle_mismatch;
    if header_corrupt {
        // Attempt to resync by scanning for a plausible ACL header
        // deeper in the buffer — well-doc'd in ISSUES.md, the
        // accumulator occasionally ends up with a 1-N byte garbage
        // prefix in front of a real ACL packet. Flushing the whole
        // buffer in that case drops the real packet too. So scan
        // forward byte-by-byte; if we know the expected handle, we
        // require an exact match on it (Pass 1) before falling back
        // to the loose criteria (Pass 2). The strict pass dramatically
        // reduces false-positive resyncs on flaky USB rings.
        let mut found_resync: Option<usize> = None;
        if let Some(active) = expected_handle {
            let mut offset = 1usize;
            while offset + 4 <= buffer.len() {
                let cand_handle_flags = u16::from_le_bytes([buffer[offset], buffer[offset + 1]]);
                let cand_handle = cand_handle_flags & 0x0fff;
                let cand_bc = (cand_handle_flags >> 14) & 0x03;
                let cand_len =
                    u16::from_le_bytes([buffer[offset + 2], buffer[offset + 3]]) as usize;
                if cand_handle == active && cand_bc == 0 && cand_len <= ACL_RESYNC_LEN_MAX {
                    found_resync = Some(offset);
                    break;
                }
                offset += 1;
            }
        }
        if found_resync.is_none() && expected_handle.is_none() {
            // Loose fallback only when we have no active handle to
            // anchor on (early bring-up before any Connection_Complete
            // event). With an active handle set, a strict-miss means
            // the real header just hasn't arrived yet — fall through
            // to the wait-for-more-bytes path below instead of
            // accepting a false positive.
            let mut offset = 1usize;
            while offset + 4 <= buffer.len() {
                let cand_handle_flags = u16::from_le_bytes([buffer[offset], buffer[offset + 1]]);
                let cand_handle = cand_handle_flags & 0x0fff;
                let cand_bc = (cand_handle_flags >> 14) & 0x03;
                let cand_len =
                    u16::from_le_bytes([buffer[offset + 2], buffer[offset + 3]]) as usize;
                if cand_handle != 0
                    && cand_handle <= 0x0eff
                    && cand_bc == 0
                    && cand_len <= ACL_RESYNC_LEN_MAX
                {
                    found_resync = Some(offset);
                    break;
                }
                offset += 1;
            }
        }
        let first32: Vec<u8> = buffer.iter().take(32).copied().collect();
        let buffer_len_before = buffer.len();
        if let Some(prefix_len) = found_resync {
            buffer.drain(..prefix_len);
            *partial_since = None;
            return AclPopOutcome::Resynced {
                dropped_prefix: prefix_len,
                declared: total_len,
                buffer_len_before,
                first32,
            };
        }
        // No plausible resync target. With an active handle we'd
        // rather wait for more bytes (the real header may still be
        // in flight) than flush blindly. The partial-stall timer
        // still fires after ACL_PARTIAL_TIMEOUT, so a permanently
        // corrupt stream still gets recovered eventually.
        if expected_handle.is_some() {
            match *partial_since {
                None => {
                    *partial_since = Some(now);
                    return AclPopOutcome::Partial {
                        declared: total_len,
                        had: buffer.len(),
                    };
                }
                Some(since) if now.duration_since(since) >= ACL_PARTIAL_TIMEOUT => {
                    let elapsed = now.duration_since(since);
                    buffer.clear();
                    *partial_since = None;
                    return AclPopOutcome::Flushed {
                        reason: AclFlushReason::PartialStall(elapsed),
                        declared: total_len,
                        had: buffer_len_before,
                        first32,
                    };
                }
                Some(_) => {
                    return AclPopOutcome::Partial {
                        declared: total_len,
                        had: buffer.len(),
                    };
                }
            }
        }
        buffer.clear();
        *partial_since = None;
        return AclPopOutcome::Flushed {
            reason: AclFlushReason::NoResyncTarget,
            declared: total_len,
            had: buffer_len_before,
            first32,
        };
    }
    if buffer.len() < total_len {
        match *partial_since {
            None => {
                *partial_since = Some(now);
                AclPopOutcome::Partial {
                    declared: total_len,
                    had: buffer.len(),
                }
            }
            Some(since) if now.duration_since(since) >= ACL_PARTIAL_TIMEOUT => {
                let first32: Vec<u8> = buffer.iter().take(32).copied().collect();
                let had = buffer.len();
                let elapsed = now.duration_since(since);
                buffer.clear();
                *partial_since = None;
                AclPopOutcome::Flushed {
                    reason: AclFlushReason::PartialStall(elapsed),
                    declared: total_len,
                    had,
                    first32,
                }
            }
            Some(_) => AclPopOutcome::Partial {
                declared: total_len,
                had: buffer.len(),
            },
        }
    } else {
        let pkt: Vec<u8> = buffer.drain(..total_len).collect();
        *partial_since = None;
        AclPopOutcome::Frame(pkt)
    }
}

/// Format the leading 32 bytes of an ACL accumulator buffer as a
/// space-separated hex string for diagnostic logging. Caps at 32 so a
/// stray full-buffer caller can't dump kilobytes into the log.
/// Persistent ACL-stream corruption detector (see the tracker locals in the
/// radio loop): ≥3 garbage-PREFIX resyncs inside 10 minutes means the dongle
/// controller is mangling its USB transfers — every connect will fail until
/// the operator power-cycles it — so raise ONE actionable hardware error.
/// ONLY Resynced (prefix-garbage) outcomes count: deterministic parser
/// stalls on one traffic shape (the MAP-poll flush loop) must never trip
/// the replug instruction (false alarm, live 2026-07-15). The window
/// emptying re-arms the report, so a relapse after a recovery is announced.
fn note_acl_corruption(
    times: &mut std::collections::VecDeque<Instant>,
    reported: &mut bool,
    event_tx: &UnboundedSender<RuntimeEvent>,
) {
    let now = Instant::now();
    while times
        .front()
        .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(600))
    {
        times.pop_front();
    }
    if times.is_empty() {
        *reported = false;
    }
    times.push_back(now);
    if times.len() >= 3 && !*reported {
        *reported = true;
        let _ = event_tx.send(RuntimeEvent::Error(
            "Bluetooth dongle USB stream corrupted (repeated garbage in reads) - connections \
             cannot succeed until the dongle is power-cycled: unplug it, wait 5 seconds, plug \
             it back in"
                .to_string(),
        ));
    }
}

fn first32_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(32)
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_runtime(
    pairing_store_path: PathBuf,
    preferred_dongle_path: Option<String>,
    event_tx: UnboundedSender<RuntimeEvent>,
    audio_tx: Sender<AudioFrame>,
    control_rx: stdmpsc::Receiver<ControlCommand>,
    status: Arc<RuntimeStatus>,
) -> Result<(), String> {
    // 1) Find and open an HCI-capable WinUSB radio. If the operator
    //    pinned a specific dongle from Pairing, prefer it; otherwise
    //    fall through to the first interface that passes the diagnose
    //    + HCI-pipe gauntlet. The fallback is intentional — a moved
    //    USB port or a swapped dongle still works without making the
    //    operator open Pairing again to update their pick.
    let interfaces = winusb::enumerate_hci_radio_interfaces()?;
    let preferred_match = preferred_dongle_path
        .as_ref()
        .and_then(|wanted| {
            let m = interfaces.iter().find(|iface| iface.path == *wanted).cloned();
            if m.is_none() {
                eprintln!(
                    "[AokieRadio] preferred dongle {:?} not currently enumerated — falling back to first HCI-capable interface",
                    wanted
                );
            }
            m
        });
    let interface = match preferred_match.or_else(|| interfaces.into_iter().next()) {
        Some(i) => {
            if let Some(wanted) = preferred_dongle_path.as_ref() {
                if i.path == *wanted {
                    eprintln!("[AokieRadio] opening operator-selected dongle: {}", i.path);
                }
            }
            i
        }
        None => {
            // No interface passed both the diagnose AND HCI-pipe checks.
            // The diag panel uses the same function, so a silent 0 here
            // when the panel sees N>0 means a transient condition (device
            // just bound, handle held by a previous probe, etc.) — surface
            // the candidate-by-candidate breakdown so we can see whether
            // it's an empty enumeration or a per-candidate diagnose
            // failure.
            let candidates = winusb::enumerate_radio_interfaces().unwrap_or_default();
            if candidates.is_empty() {
                return Err("no WinUSB Bluetooth dongle visible to the OS — install \
                     WinUSB on your dongle via Settings → Pairing first \
                     (or replug if you just installed)"
                    .to_string());
            }
            let mut details = String::new();
            for c in &candidates {
                match winusb::diagnose_interface_path(&c.path) {
                    Ok(d) => {
                        let p = &d.classified;
                        details.push_str(&format!(
                            "\n  [{:?}] event_in={} acl_in={} acl_out={} sco_in={} sco_out={} — {}",
                            c.source,
                            p.event_in.is_some(),
                            p.acl_in.is_some(),
                            p.acl_out.is_some(),
                            p.sco_in.is_some(),
                            p.sco_out.is_some(),
                            c.path,
                        ));
                    }
                    Err(e) => {
                        details.push_str(&format!(
                            "\n  [{:?}] diagnose failed: {} — {}",
                            c.source, e, c.path,
                        ));
                    }
                }
            }
            return Err(format!(
                "no HCI-capable WinUSB radio interface found among {} candidate(s):{}",
                candidates.len(),
                details,
            ));
        }
    };
    let mut transport = AokieHciTransport::open(&interface.path)?;

    // 2) Boot the controller (reset, name, COD, scan-enable, SSP, etc.)
    //    Same sequence the diag runtime uses.
    let init_report = manager::initialize_transport(&interface.path, &transport)?;
    // Tight timeouts so the main loop iterates fast enough to keep up
    // with SCO's 8 kHz audio frame rate during a call. A 60-byte SCO
    // payload is only ~3.75 ms of audio, so we need ~270 packets/sec
    // of TX throughput; 5 ms reads + ~32 packets per tick achieves
    // that. Outside of a call the same loop services control commands
    // (Answer / Hangup) and stays responsive.
    transport.set_read_timeouts(5, Some(5), Some(5))?;

    status
        .addresses
        .write()
        .map(|mut a| a.local = init_report.local_address.clone())
        .ok();
    status.initialized.store(true, Ordering::Relaxed);
    let _ = event_tx.send(RuntimeEvent::Initialized(init_report.local_address.clone()));

    // 3) Pairing store — lives across runtime restarts so a previously
    //    paired phone reconnects without prompting again.
    let mut pairing_store = AokiePairingStore::load(pairing_store_path)?;
    // AOK-BT-001: publish the initial bonded-device snapshot so phone.listPaired
    // reflects the store from the first RPC.
    if let Ok(mut snap) = status.bonded_devices.write() {
        *snap = pairing_store.list_devices();
    }
    // Track the advertised scan-enable so the loop only writes the controller when
    // the pairing window actually changes state (opened / expired / closed). The
    // controller came up connectable-only (AOK-BT-001) — not discoverable yet.
    let mut scan_discoverable = false;
    // PAIR-001: legacy fixed-PIN ("0000") pairing is OFF unless the operator
    // deliberately enabled the compat setting (plugin maps `legacyPairingPin`
    // to this env var at radio start; window-gated like every other bond step).
    let legacy_pin_allowed = std::env::var("AOKIE_LEGACY_PAIRING_PIN").as_deref() == Ok("1");
    if legacy_pin_allowed {
        eprintln!(
            "[AokieRadio] ⚠️ legacyPairingPin compat mode ENABLED — fixed-PIN pairing \
             provides no authentication; disable it once the device is bonded"
        );
    }
    // PAIR-001: the held SSP numeric comparison (address + operator deadline).
    // Mirrored into status.pairing_confirm for phone.status; the loop answers
    // negatively on expiry / window close / peer disconnect.
    let mut pending_ssp_confirm: Option<(String, Instant)> = None;

    // 4) Loop state, mirrored from `listen_runtime_controller_with_options`.
    let max_acl_len = manager::max_acl_packet_len(&init_report.buffer_size);
    let max_sco_len = manager::max_sco_packet_len(&init_report.buffer_size);
    let mut l2cap_state = l2cap::L2capState::new();
    // Phase 4e: MAP MNS server lives across the full runtime — phones
    // connect inbound RFCOMM on AOKIE_MNS_RFCOMM_CHANNEL and push
    // EventReports through the shared MnsServer, which the main loop
    // drains on every tick. Reset (not rebuilt) on ACL disconnect so
    // the same Arc references stay valid in the L2CAP closures.
    let mns_server: Arc<StdMutex<MnsServer>> = Arc::new(StdMutex::new(MnsServer::new()));
    // wbs_supported = "advertise mSBC alongside CVSD in AT+BAC, so the
    // phone can pick wide-band speech."
    //
    // Default: derived from `transport.supports_msbc_alt_setting()`.
    // BTstack-parity (post-bf95183): mSBC over USB uses voice setting
    // 0x0043 (transparent + 8-bit input) and rides alt 1 (MPS=9) for
    // one connection. The 24-byte HCI SCO USB-transport payload
    // override means alt 1 is fine for mSBC — the H2 sync triplet
    // drives reassembly. Any dongle that exposes any of the 8-bit
    // alts (1/2/3) supports mSBC.
    //
    // Override via `AOKIE_HFP_CODEC` env var:
    //   - `wbs` / `msbc`        → force mSBC + CVSD advertised
    //   - `cvsd` / `narrowband` → force CVSD-only
    //   - unset / `auto`        → use transport probe (default)
    //
    // Why the override exists: some dongles' CVSD encoder is
    // non-functional in firmware. On those dongles the CVSD-only
    // fallback flips voice_setting to 0x0060 (controller-side CVSD
    // encode); if the encoder is broken, no bytes hit the air and the
    // call goes silent. mSBC mode uses 0x0043 (transparent + 8-bit
    // input — controller passes our pre-encoded bytes through
    // verbatim), which sidesteps the broken encoder. Set
    // `AOKIE_HFP_CODEC=wbs` to force mSBC on those.
    let codec_override = std::env::var("AOKIE_HFP_CODEC")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase());
    let wbs_supported = match codec_override.as_deref() {
        Some("wbs") | Some("msbc") => {
            eprintln!(
                "[AokieRadio] AOKIE_HFP_CODEC=wbs — forcing mSBC + CVSD advertisement (transport probe overridden)"
            );
            true
        }
        Some("cvsd") | Some("narrowband") => {
            eprintln!(
                "[AokieRadio] AOKIE_HFP_CODEC=cvsd — forcing CVSD-only advertisement (transport probe overridden)"
            );
            false
        }
        _ => {
            let probed = transport.supports_msbc_alt_setting();
            if !probed {
                eprintln!(
                    "[AokieRadio] transport does not expose mSBC alt-setting — advertising CVSD-only in AT+BAC (override with AOKIE_HFP_CODEC=wbs)"
                );
            }
            probed
        }
    };
    // Phase 4: whether this process advertises HFP call waiting / 3-way
    // (BRSF bit 1) and probes AT+CHLD=? / AT+CCWA=1 at SLC time. Derived
    // from the plugin's holdAndCallWaiting setting exactly like the other
    // radio-start env vars; default off keeps the legacy wire behaviour.
    let call_waiting_enabled = std::env::var_os("AOKIE_CALL_WAITING").is_some();
    if call_waiting_enabled {
        eprintln!(
            "[AokieRadio] AOKIE_CALL_WAITING set — advertising call waiting / 3-way in BRSF and probing AT+CHLD=? / AT+CCWA=1"
        );
    }
    install_mns_server_channel(
        &mut l2cap_state,
        mns_server.clone(),
        wbs_supported,
        call_waiting_enabled,
    );
    let mut sco_assembler = sco::ScoPacketAssembler::new();
    let mut sco_tx_queue = sco::LinearPcmTxQueue::new(AOKIE_SCO_TX_QUEUE_SAMPLES);
    // Track inbound SCO liveness so we can spot the case where the
    // controller raises the link but never delivers any payload — that
    // looks the same as "phone muted its mic" in the higher layers but
    // is usually a WinUSB isoch endpoint not actually arming. Stamped
    // on every read_sco that returns >0 bytes; checked once per second
    // while SCO is up.
    let mut last_sco_rx_bytes_at: Option<Instant> = None;
    let mut sco_rx_silence_logged_at: Option<Instant> = None;
    // mSBC codec state. The framer reassembles 60-byte H2 frames from
    // arbitrarily-chunked HCI SCO payloads (controllers fragment based
    // on their HCI buffer size, not the air slot size); the packager
    // owns the encoder and exposes a byte-stream view so we can drain
    // HCI-buffer-sized chunks. Reset on every disconnection so the H2
    // sequence number and partial frame buffers don't bleed between
    // calls.
    let mut h2_decoder = H2Decoder::new();
    let mut msbc_rx_framer = MsbcStreamFramer::new();
    let mut msbc_tx_packager = MsbcStreamPackager::new();
    let mut msbc_rx_diag = MsbcRxDiag::new();
    // The Tauri layer drives the auto-answer timing (configurable delay
    // from the UI), so the in-runtime auto-answer stays off. Otherwise
    // we'd race the Tauri layer and ATA twice for a single ring.
    let mut hfp_control = manager::HfpControlReport {
        auto_answer_enabled: false,
        ..Default::default()
    };
    // `hfp_call_control_packets_for_event` updates this to track whether
    // we've already ATA'd this incoming call so a duplicate Ringing /
    // IncomingCall doesn't try to answer twice. The Tauri layer drives
    // the auto-answer above so this stays a defensive guard.
    let mut answer_sent_for_call = false;
    let mut selected_codec: Option<(String, u16)> = None;
    let mut active_sco_handle: Option<u16> = None;
    // Phase 3e: track the live ACL handle so we can pass it to
    // `PbapRuntime::new` once HFP service-level connection completes.
    // Cleared on ACL DisconnectionComplete; SCO disconnects don't
    // touch it.
    let mut active_acl_handle: Option<u16> = None;
    // Phase 3e: at most one PBAP fetch attempt in flight per ACL
    // connection. Constructed on `HfpEvent::ServiceLevelConnectionReady`
    // and dropped when the state machine reaches `Done` / `Failed`
    // OR when the ACL link goes away. Holding it across SLC events
    // would otherwise let a glitching AG that re-fires SLC kick off
    // a parallel fetch — annoying but harmless, except we also lose
    // the L2CAP channel CIDs the previous attempt allocated.
    let mut pbap_runtime: Option<PbapRuntime> = None;
    // HARD-001 outbound reconnect (`phone.connect`). `manual_connect_pending`
    // = a Create_Connection we issued and are awaiting ConnectionComplete
    // for (addr + when, for the page-budget watchdog). Once the ACL lands,
    // `hfp_connect_runtime` drives SDP → RFCOMM → SLC kickoff, and
    // `outbound_connect_session` stays true for the life of that ACL so an
    // SLC failure tears the link down (a paged link with no SLC is a dead
    // half-link the phone won't repair on its own — the old
    // AUTO_RECONNECT_OUTBOUND_PAGE symptom).
    let mut manual_connect_pending: Option<(String, Instant)> = None;
    // After the page lands, WE must drive LMP authentication + encryption
    // before any profile L2CAP traffic — on the inbound path the phone (as
    // the paging master) does this, and a Security-Mode-4 phone that sees
    // unauthenticated RFCOMM from us tears the link down with 0x05. Tracks
    // the Authentication_Requested → Set_Connection_Encryption handshake;
    // `awaiting_encryption` = auth done, Encryption_Change pending. SDP
    // starts only once the link is encrypted.
    let mut manual_connect_auth: Option<ManualConnectAuth> = None;
    let mut hfp_connect_runtime: Option<HfpConnectRuntime> = None;
    let mut outbound_connect_session = false;
    // Phase 4e: a single MapRuntime slot for sequential MAS operations
    // (subscribe, fetch new SMS body, push reply). Each MapRuntime
    // tears its own L2CAP/RFCOMM channels up and down; running two
    // concurrently would race the L2CAP CID allocator and confuse
    // the AG. New ops queue and drain through `pending_map_ops`.
    let mut map_runtime: Option<MapRuntime> = None;
    let mut pending_map_ops: VecDeque<PendingMapOp> = VecDeque::new();
    // Tracks which queued op the active MapRuntime is fulfilling, so
    // OperationCompleted knows which event to emit (Subscribe →
    // MapNotificationsSubscribed, FetchMessage → SmsReceived,
    // SendReply → SmsSent).
    let mut active_map_op: Option<PendingMapOp> = None;
    // Phase 4g.d: stamp set when the pooled MAP runtime parks in
    // Resting with no queued op. After MAP_IDLE_TIMEOUT we force a
    // DISCONNECT so we don't sit on an open RFCOMM channel forever.
    // None when we're either driving an op or resting with a queue.
    let mut map_idle_since: Option<std::time::Instant> = None;
    // Phase 4e: dedupe SLC-fired Subscribe enqueues. Reset on ACL
    // disconnect so the next pairing re-subscribes.
    let mut mns_subscription_attempted_for_acl = false;
    // When the Subscribe was queued — the MNS-absence grace clock. On
    // outbound (initiator-mux) sessions the phone cannot open its MNS
    // notification DLCI toward us yet, so MNS stays AwaitingConnect for
    // the whole ACL; once this clock passes MNS_ABSENT_POLL_GRACE the
    // inbox poll stops waiting for MNS and becomes the inbound-SMS
    // channel itself (live 2026-07-13: a customer's reply sat unread
    // for 25 minutes because the poll was gated on MNS Connected).
    let mut mns_subscribe_attempted_at: Option<Instant> = None;
    // Inbox-poll fallback for dropped MNS pushes. seen_handles tracks
    // every handle we've ever queued FetchMessage for in this ACL
    // session so neither MNS nor poll double-fetches. inbox_poll_seeded
    // flips true after the first successful poll — its job is to
    // populate seen_handles with the existing inbox without auto-
    // replying to every historical row. last_inbox_poll caps the
    // cadence; cleared on ACL drop because handles are AG-scoped.
    let mut seen_handles: HashSet<String> = HashSet::new();
    let mut inbox_poll_seeded = false;
    let mut last_inbox_poll: Option<Instant> = None;
    // Counter incremented every time we queue a PollInbox while
    // inbox_poll_seeded is false. Lets the listing handler tell apart
    // the very first poll (clean app start — seed all, fetch nothing)
    // from a retry after a previous failed seed (also fetch the top
    // entry, since the customer's message that arrived during the
    // dead session is most likely sitting at the top of the listing).
    // Preserved across ACL disconnect so a Pixel-recovery cycle's
    // next session knows it's the retry, not a fresh start.
    let mut seed_attempts: usize = 0;
    // Stamped when a SendReply op completes (AG ack'd the PUT). The
    // PollInbox scheduler gates on this: empirically, issuing a
    // SETPATH on dlci 11 within ~tens of seconds of a successful PUT
    // to /outbox makes Pixel's MAS go silent for the rest of the ACL
    // (the catatonic-mux symptom). Best guess is the AG is still busy
    // committing the just-PUT SMS to its cellular queue / SMS DB and
    // drops the next OBEX request rather than queueing it. Skipping
    // PollInbox in that window leans on MNS to deliver fresh messages
    // (which keeps working since dlci 4 isn't being poked) and only
    // falls back to polling once the AG has had time to settle. See
    // `project_pixel_rfcomm_mux_catatonia.md`.
    let mut last_send_reply_at: Option<Instant> = None;
    // Rate-limits the "PollInbox deferred — SendReply was Xs ago" log
    // so the same condition doesn't print every loop iteration. Only
    // useful for visibility; doesn't affect gate behaviour.
    let mut last_poll_skip_log_at: Option<Instant> = None;
    // PBAP runs AFTER MAP Subscribe completes (gated on MNS Connected). On Pixel,
    // PBAP body streaming sometimes leaves the BT stack catatonic — if PBAP ran
    // first and confused the phone, MAP's subsequent SDP query stalled. Inverting
    // the order lets MAP get its L2CAP/RFCOMM/MNS state up while Pixel is healthy.
    //
    // …and even with that ordering, Pixel's PSE flips SRM=enable on the first
    // chunk and our PCE doesn't honor SRM. The phone stops streaming, our 15-second
    // watchdog fires, and meanwhile Pixel times out our MAS session on its side
    // — so PBAP's failure mode actually breaks SMS delivery as well. Until SRM
    // is honored properly, skip the auto-fetch and rely on the SQLite contact
    // cache for caller-name lookup. See `project_pbap_srm_pse` memory entry.
    const PBAP_AUTO_FETCH_ON_RECONNECT: bool = false;
    let mut pbap_pending_for_acl = false;
    // MAS-stall recovery (2026-04-28). Pixel's RFCOMM mux periodically
    // goes catatonic mid-session — one chunk of an MNS PUT arrives, then
    // nothing on dlci 4 (MNS) or dlci 11 (MAS) for the rest of the
    // ACL. Subsequent PollInbox/FetchMessage SETPATHs go out and never
    // get a response. The recovery is to force-DISC both DLCIs and let
    // Pixel reopen them fresh: it's the only thing observed to unstick
    // the phone short of an ACL re-pair.
    //
    // Detection: `last_acl_inbound_at` is stamped on every successful
    // L2CAP handle_acl_packet AND on raw byte arrival from the ACL
    // bulk endpoint. The byte-level stamp matters because Pixel can
    // mid-stream a long inbox listing into the buffer for tens of
    // seconds without any chunk ever being a complete L2CAP packet
    // — that's bytes-alive, frame-stuck, and we mustn't HCI-Disconnect
    // an incoming call just because our parser hasn't drained. When
    // MAP work is pending or active and we haven't seen any inbound
    // ACL bytes OR frames for `MAS_STALL_THRESHOLD`, the watchdog
    // fires. The threshold is generous enough to ride out healthy
    // idle periods (HFP keepalives don't fire here so the signal is
    // genuinely "Pixel went silent") but short enough that the user
    // notices a stuck-MAS in a usable timeframe. Healthy OBEX
    // exchanges complete in under a second, so 15 s is well past
    // the failure threshold for a normal listing; a stalled
    // PollInbox will trip this in the customer-visible window
    // (vs. the original 35 s, which left them waiting too long).
    const MAS_STALL_THRESHOLD: Duration = Duration::from_secs(15);
    let mut last_acl_inbound_at: Instant = Instant::now();
    // Stage-2 escalation: stage-1 (force-DISC dlci 11+4) has never
    // been observed to actually unstick Pixel — every wild test shows
    // mns staying AwaitingConnect with no fresh SDP query coming
    // back. We keep stage-1 in place because it costs almost nothing
    // and *might* recover other vendors' phones, but on Pixel the
    // useful work happens at stage-2. 5 s is long enough to give
    // stage-1 a fair shot before we tear the ACL down; the previous
    // 20 s window just delayed the customer-visible recovery.
    const MAS_STALL_ESCALATION: Duration = Duration::from_secs(5);
    let mut mas_recovery_attempted_at: Option<Instant> = None;
    // When the CURRENT active_map_op went in flight — the per-op
    // deadline clock (see the op_overdue check in the watchdog).
    let mut active_map_op_since: Option<Instant> = None;
    // Tracks the "MAP work pending" state across loop iterations so
    // we can detect the idle→active edge. When work transitions from
    // none to some, we reset `last_acl_inbound_at` so the stall
    // watchdog starts fresh — otherwise a healthy 19 s quiet period
    // before queueing a poll would make the watchdog fire instantly,
    // before Pixel had a chance to respond to the just-sent request.
    let mut prev_map_work_pending: bool = false;
    // Fallback: if MAP Subscribe doesn't complete within 10s of SLC ready, run PBAP
    // anyway so we still get caller-name lookup even when MAP is borked.
    let mut pbap_pending_since: Option<Instant> = None;
    // Air packet length the SCO link negotiated, captured from the
    // tx_packet_length parameter of Synchronous_Connection_Complete.
    // We clamp our HCI SCO payload to this so each HCI packet maps to
    // exactly one air packet (no controller-side fragmentation). Reset
    // on Disconnection_Complete for the SCO handle.
    let mut active_sco_tx_packet_len: Option<usize> = None;
    // When the next SCO TX packet should be allowed to leave. Used
    // to pace writes to wall-clock at `payload_len / 16` ms / packet
    // (8 kHz CVSD = 16 audio bytes per ms) so we don't flood the
    // controller's outgoing SCO buffer faster than it can drain.
    let mut sco_tx_next_packet_at: Option<Instant> = None;
    // ACL stream accumulator. WinUSB delivers ACL bytes as a stream:
    // a single bulk read can return either a tail of an ACL packet
    // whose head we already consumed, or the head of one packet plus
    // the start of the next. Parsing the read buffer as one ACL packet
    // therefore desyncs us as soon as the controller batches two short
    // packets into one transfer (or splits a larger one across two).
    // We buffer raw bytes here and drain complete `[handle:2|len:2|payload:len]`
    // packets out of it; a partial tail stays in the buffer until the
    // next read completes it.
    let mut acl_buffer: Vec<u8> = Vec::new();
    // When the parser first sees a "partial packet" (header declares
    // more bytes than the buffer holds yet), stamp here. Cleared on
    // every successful drain. If a partial state lingers — e.g. our
    // buffer is misaligned and reading payload bytes as a header that
    // claims thousands of bytes that will never arrive — the stale
    // detector below dumps the head and flushes so the next inbound
    // packet can resync. Without this, a one-time misalignment wedges
    // the entire ACL stream forever (incoming-call RING bytes get
    // queued behind the bogus header and HFP never sees them).
    let mut acl_partial_since: Option<Instant> = None;
    // Persistent-corruption detector (live incident 2026-07-15): a wedged
    // dongle controller (BCM20702 firmware) prefixed EVERY inbound transfer
    // with garbage bytes until a PHYSICAL replug — software resets, HCI
    // reset and USB disable/enable all survived it, and connections just
    // failed silently for hours. Repeated resync/flush recoveries in a
    // short window are that signature; surface ONE actionable hardware
    // error (degraded health + operator toast) instead of a mute death.
    // A transient one-off resync (seen once during a 2026-07-13 listing
    // burst) stays a log line — it never reaches the threshold.
    let mut acl_corruption_times: std::collections::VecDeque<Instant> = Default::default();
    let mut acl_corruption_reported = false;
    let mut loop_iter: u64 = 0;
    let mut last_heartbeat = Instant::now();
    // ACL keepalive: during a call the SCO link monopolises the air and the AG
    // stops sending ACL, so if nothing is received the controller hits the
    // link-supervision timeout (Disconnection reason 0x08) and drops the whole
    // connection mid-call. We poll AT+CIND? when the ACL goes quiet to elicit a
    // response and reset the supervision timer.
    let mut last_hfp_keepalive = Instant::now();

    // ── Auto-reconnect to paired devices ─────────────────────────────────
    //
    // The receptionist would otherwise sit in passive Page Scan and wait
    // for the phone to initiate the connection. Phones don't reliably do
    // that — most stacks page their host once on startup and then leave
    // it to the user to tap "connect" in Bluetooth settings. So we drive
    // it from our end: when no ACL is up, periodically issue
    // HCI_Create_Connection to each paired BD_ADDR.
    //
    // State:
    //  - `auto_reconnect_target_index`  — round-robins through the
    //    pairing-store list so a phone that's been swapped doesn't
    //    permanently starve out a newer one.
    //  - `auto_reconnect_pending_since` — when `Some`, a Create_Connection
    //    is in flight and we're waiting for HCI Connection Complete.
    //    Cleared either by ConnectionComplete handling below or by the
    //    page-budget watchdog if the controller silently dropped it.
    //  - `auto_reconnect_next_attempt`  — earliest `Instant` we'll fire
    //    the next page. Stamps forward on every attempt + on every
    //    completion (success OR failure).
    //
    // The interval is intentionally longer than the HCI page timeout
    // (~15s) so a failed page can complete (= fall through Connection
    // Complete with non-zero status) before we try the next addr —
    // overlapping pages would burn host buffers and confuse the
    // controller's link manager.
    //
    // **Feature gate (2026-04-28)**: outbound paging is currently OFF
    // because Pixel/Bluedroid does *not* auto-initiate profile setup
    // (SDP / RFCOMM / HFP SLC) when the AG is the ACL slave. Two
    // separate test runs confirmed: page succeeded → ConnectionComplete
    // status=0 → L2CAP InformationRequest exchanged → then total dead
    // air for the rest of the session. We then added a follow-up
    // HCI Switch_Role(slave) so the AG would become master after our
    // page; that *also* failed to wake the phone's auto-profile path.
    // Net effect of leaving auto-reconnect on: the phone won't reconnect
    // by itself either (the half-up ACL link discourages it), so the
    // user sees a "connected, no traffic" link instead of the "tap
    // phone to connect" prompt they're used to.
    //
    // Until we drive SDP+RFCOMM ourselves after an outbound page (real
    // fix: ~200 lines of state machine glue), keep this off and let
    // the user re-tap the phone. The Switch_Role helper stays in
    // `hci.rs` and the call site below stays gated by the same flag —
    // they'll be useful again once profile-driving is in place.
    const AUTO_RECONNECT_OUTBOUND_PAGE: bool = false;
    const AUTO_RECONNECT_INTERVAL: Duration = Duration::from_secs(20);
    /// If we sent Create_Connection but never saw Connection Complete
    /// after this long, give up internally and try again. Slightly
    /// longer than the HCI page timeout we configured (0x6000 slots ≈
    /// 15.4s) so the controller has time to surface its own timeout
    /// first.
    const AUTO_RECONNECT_PAGE_BUDGET: Duration = Duration::from_secs(18);
    /// HARD-001 phone.connect: authentication + encryption on the link we
    /// paged must complete within this budget or we tear the ACL down (a
    /// paged-but-unsecured link is a dead half-link the phone won't repair).
    const MANUAL_CONNECT_AUTH_BUDGET: Duration = Duration::from_secs(10);
    let mut auto_reconnect_target_index: usize = 0;
    let mut auto_reconnect_pending_since: Option<Instant> = None;
    let mut auto_reconnect_pending_addr: Option<String> = None;
    // Don't start hammering the controller while it's still finishing
    // post-Reset configuration. By heartbeat tick #1 (~2s in) the
    // manager has finished init and we're ready to page.
    let mut auto_reconnect_next_attempt: Option<Instant> =
        Some(Instant::now() + Duration::from_secs(3));

    loop {
        loop_iter += 1;
        // Flush stderr (our log stream) every iteration so the last log
        // line before any crash is actually visible — saves us from "we
        // don't know where the silent thread went" when a panic / stack
        // overflow hits between buffer flushes. (Never touch stdout: when
        // this runtime runs inside the aokie-plugin, stdout is the NDJSON
        // protocol channel and must stay clean.)
        let _ = std::io::stderr().flush();
        if last_heartbeat.elapsed() >= Duration::from_secs(2) {
            last_heartbeat = Instant::now();
            // Extra MAP/MNS diagnostics surface only while there's an
            // active ACL — saves log noise during the long pre-pair
            // pre-roll. The post-SendReply silence window is the one we
            // really want visibility on (project_pixel_rfcomm_mux_catatonia,
            // project_map_rcs_limitation): if MNS reports Connected and
            // last_send_reply_at is set, but no inbound traffic for tens
            // of seconds, we're either looking at a Pixel-side wedge or
            // RCS swallowing the customer's follow-up.
            let map_diag = if active_acl_handle.is_some() {
                let mns_state = mns_server
                    .lock()
                    .map(|g| format!("{:?}", g.state()))
                    .unwrap_or_else(|_| "poisoned".to_string());
                let since_inbound = last_acl_inbound_at.elapsed().as_secs();
                let since_reply = last_send_reply_at
                    .map(|t| format!("{}s", t.elapsed().as_secs()))
                    .unwrap_or_else(|| "n/a".to_string());
                format!(
                    " mns={} pending_map={} active_map={} acl_in={}s_ago reply_ack={}",
                    mns_state,
                    pending_map_ops.len(),
                    active_map_op.is_some() as u8,
                    since_inbound,
                    since_reply
                )
            } else {
                String::new()
            };
            eprintln!(
                "[AokieRadio] heartbeat iter={} sco_active={} sco_tx_queue={} samples{}",
                loop_iter,
                active_sco_handle.is_some(),
                sco_tx_queue.len(),
                map_diag,
            );
            // ACL keepalive (see decl above): once the ACL has been quiet for a
            // few seconds while connected, send AT+CIND? to draw a +CIND reply,
            // which resets the link-supervision timer and holds the call up.
            //
            // MUCH more aggressive while a call's SCO is up (observed
            // 2026-07-13: two consecutive calls died mid-reply with reason
            // 0x08 at ~8-10s of ACL quiet — the RX path faded during long
            // sustained mSBC TTS transmits, and the single ~7s keepalive was
            // the link's only ARQ-retransmitted traffic before both
            // supervision timers gave up). ACL packets retry until
            // acknowledged, so on a marginal link a 2s CIND? cadence is
            // cheap insurance that keeps BOTH sides' supervision timers fed
            // through a fade the fixed-rate SCO stream can't survive alone.
            // Idle (no-call) links keep the old lazy cadence.
            let (keepalive_idle, keepalive_gap) = if active_sco_handle.is_some() {
                (Duration::from_secs(2), Duration::from_secs(2))
            } else {
                (Duration::from_secs(6), Duration::from_secs(4))
            };
            if active_acl_handle.is_some()
                && last_acl_inbound_at.elapsed() >= keepalive_idle
                && last_hfp_keepalive.elapsed() >= keepalive_gap
            {
                match l2cap_state
                    .build_hfp_call_control_packets(HfpAtCommand::RetrieveIndicatorStatus)
                {
                    Ok(packets) if !packets.is_empty() => {
                        for packet in &packets {
                            let _ = transport.write_acl(packet);
                        }
                        last_hfp_keepalive = Instant::now();
                        eprintln!(
                            "[AokieRadio] ACL idle {}s — sent AT+CIND? keepalive to hold the link-supervision timer",
                            last_acl_inbound_at.elapsed().as_secs()
                        );
                    }
                    // No HFP SLC yet (pre-pair) or nothing to send — skip quietly.
                    _ => {}
                }
            }
            // SCO RX liveness check. If the link is up but we've never
            // received a single byte (or it's been >2 s since the last
            // one), surface that — once per silent period, not every
            // heartbeat. This distinguishes "the phone muted its mic"
            // (RX bytes flowing but the audio is silence; mSBC RX diag
            // would still log) from "the WinUSB isoch endpoint isn't
            // actually delivering anything" (no read_sco data ever).
            if active_sco_handle.is_some() {
                let silent_for = last_sco_rx_bytes_at
                    .map(|t| t.elapsed())
                    .unwrap_or(Duration::from_secs(u64::MAX));
                let should_log = silent_for >= Duration::from_secs(2)
                    && sco_rx_silence_logged_at
                        .map(|t| t.elapsed() >= Duration::from_secs(5))
                        .unwrap_or(true);
                if should_log {
                    if last_sco_rx_bytes_at.is_none() {
                        eprintln!("[AokieRadio] SCO RX silent: 0 inbound bytes since link up");
                    } else {
                        eprintln!(
                            "[AokieRadio] SCO RX silent: no inbound bytes for {:?}",
                            silent_for
                        );
                    }
                    sco_rx_silence_logged_at = Some(Instant::now());
                }
            }
            // SLC stall watchdog. 10 s is generous: BRSF/CIND/CHLD
            // exchanges complete in milliseconds when the AG is healthy;
            // a stall this long means the AG dropped a reply (or the
            // ACL link is in trouble). The HFP spec doesn't define a
            // precise timeout for individual AT replies, but most
            // headsets give up around 5–15 s.
            let now = Instant::now();
            l2cap_state.tick_hfp_stalls(now, Duration::from_secs(10));
            // L2CAP-level watchdog: tear down channels stuck in
            // Configuring (peer never sent our ConfigureResponse).
            // Healthy ConfigureResponses arrive in ~10s of ms, so 5 s
            // is a long way past the failure threshold while still
            // bounded enough to free the call attempt before the user
            // gives up. Returns courtesy DisconnectionRequest packets.
            let stall_packets = l2cap_state.tick_l2cap_stalls(now, Duration::from_secs(5));
            for packet in &stall_packets {
                if let Err(e) = transport.write_acl(packet) {
                    let _ = event_tx.send(RuntimeEvent::Error(format!(
                        "stall teardown ACL write: {}",
                        e
                    )));
                }
            }
            // Drain whatever the watchdogs surfaced (HFP-stall events
            // pushed into channel queues, plus L2CAP-stall events
            // pushed into the orphan queue) so the manager learns
            // about the failure now instead of on the next inbound
            // ACL packet — which may never arrive if the peer is
            // genuinely dead.
            for hfp_event in l2cap_state.take_hfp_events() {
                manager::update_selected_codec(&mut selected_codec, &hfp_event);
                if let Err(e) = apply_codec_voice_setting(&transport, &hfp_event) {
                    let _ =
                        event_tx.send(RuntimeEvent::Error(format!("voice-setting switch: {}", e)));
                }
                let control_packets = manager::hfp_call_control_packets_for_event(
                    &mut l2cap_state,
                    &hfp_event,
                    &mut answer_sent_for_call,
                    &mut hfp_control,
                )?;
                for packet in &control_packets {
                    if let Err(e) = transport.write_acl(packet) {
                        let _ = event_tx.send(RuntimeEvent::Error(format!("stall drain: {}", e)));
                    }
                }
                // HARD-001: an SLC failure on a link WE paged is a dead
                // half-link — tear the ACL down so the UI never shows a
                // "connected" phone that can't take calls.
                if outbound_connect_session {
                    if let HfpEvent::ServiceLevelConnectionFailed(_) = &hfp_event {
                        if let Some(handle) = active_acl_handle {
                            eprintln!(
                                "[AokieRadio] phone.connect: SLC failed on our paged link — disconnecting handle {:#06x}",
                                handle
                            );
                            let _ = transport.write_command(&hci::disconnect_command(handle, 0x13));
                        }
                        outbound_connect_session = false;
                    }
                }
                // MAP subscribe on SLC ready — SAME trigger as the
                // inbound-ACL drain below. WHICH drain surfaces the
                // ready event is timing-dependent (an outbound
                // session's final SLC OK routinely lands here), and
                // before 2026-07-13 only the other drain queued the
                // Subscribe — so outbound-connected phones never got
                // MAP/MNS at all. The attempted-for-acl guard keeps the
                // two sites from double-queueing.
                if matches!(hfp_event, HfpEvent::ServiceLevelConnectionReady)
                    && !mns_subscription_attempted_for_acl
                {
                    pending_map_ops.push_front(PendingMapOp::Subscribe);
                    mns_subscription_attempted_for_acl = true;
                    mns_subscribe_attempted_at = Some(Instant::now());
                    if PBAP_AUTO_FETCH_ON_RECONNECT {
                        pbap_pending_for_acl = true;
                        pbap_pending_since = Some(Instant::now());
                    }
                }
                forward_hfp_event(hfp_event, &event_tx, &status, &interface.path);
            }

            // ── Auto-reconnect tick ──────────────────────────────────
            //
            // Two checks fire here, in order:
            //
            //   1. Page-budget watchdog — clears stale `pending_*` state
            //      if the controller never produced Connection Complete
            //      for a Create_Connection we sent N seconds ago. This
            //      keeps the loop unwedged if the controller silently
            //      dropped the command (link manager ENOMEM / dongle
            //      reset).
            //
            //   2. Schedule the next page if all of these hold:
            //        - no ACL link is up (active_acl_handle.is_none())
            //        - no page is in flight (auto_reconnect_pending_*)
            //        - the next-attempt deadline has elapsed
            //        - the pairing store has at least one addr
            //
            //      We round-robin through the addr list so a swapped
            //      phone doesn't permanently starve a newer pairing.
            if let Some(started) = auto_reconnect_pending_since {
                if started.elapsed() >= AUTO_RECONNECT_PAGE_BUDGET {
                    if let Some(addr) = auto_reconnect_pending_addr.take() {
                        eprintln!(
                            "[AokieRadio] auto-reconnect page budget elapsed for {} — \
                             treating as page timeout, will retry on next interval",
                            addr
                        );
                    }
                    auto_reconnect_pending_since = None;
                    auto_reconnect_next_attempt = Some(Instant::now() + AUTO_RECONNECT_INTERVAL);
                }
            }

            if AUTO_RECONNECT_OUTBOUND_PAGE
                && active_acl_handle.is_none()
                && auto_reconnect_pending_since.is_none()
            {
                let due = auto_reconnect_next_attempt
                    .map(|t| Instant::now() >= t)
                    .unwrap_or(true);
                if due {
                    let paired = pairing_store.list_addresses();
                    if paired.is_empty() {
                        // Nothing to reconnect to — push the next attempt
                        // out so we don't busy-poll on an empty store
                        // every heartbeat.
                        auto_reconnect_next_attempt =
                            Some(Instant::now() + AUTO_RECONNECT_INTERVAL);
                    } else {
                        let idx = auto_reconnect_target_index % paired.len();
                        auto_reconnect_target_index = idx.wrapping_add(1);
                        let addr = paired[idx].clone();
                        match hci::create_connection_command_default(&addr) {
                            Ok(cmd) => match transport.write_command(&cmd) {
                                Ok(()) => {
                                    eprintln!(
                                        "[AokieRadio] auto-reconnect: paging paired \
                                         device {} (HCI Create_Connection)",
                                        addr
                                    );
                                    auto_reconnect_pending_addr = Some(addr);
                                    auto_reconnect_pending_since = Some(Instant::now());
                                    // Stamp next_attempt now so even if Connection
                                    // Complete never fires, the watchdog branch
                                    // above will fall through to a clean retry.
                                    auto_reconnect_next_attempt =
                                        Some(Instant::now() + AUTO_RECONNECT_INTERVAL);
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[AokieRadio] auto-reconnect: HCI \
                                         Create_Connection write to {} failed: {}",
                                        addr, e
                                    );
                                    auto_reconnect_next_attempt =
                                        Some(Instant::now() + AUTO_RECONNECT_INTERVAL);
                                }
                            },
                            Err(e) => {
                                eprintln!(
                                    "[AokieRadio] auto-reconnect: bad BD_ADDR {} \
                                     in pairing store: {}",
                                    addr, e
                                );
                                auto_reconnect_next_attempt =
                                    Some(Instant::now() + AUTO_RECONNECT_INTERVAL);
                            }
                        }
                    }
                }
            }

            // ── phone.connect page-budget watchdog ──────────────────
            // A Create_Connection we issued for a manual reconnect that
            // never produced ConnectionComplete (dongle dropped it /
            // phone out of range with a controller that stays silent).
            // Clear the pending slot so the next phone.connect isn't
            // refused as "already in progress".
            if let Some((addr, started)) = &manual_connect_pending {
                if started.elapsed() >= AUTO_RECONNECT_PAGE_BUDGET {
                    eprintln!(
                        "[AokieRadio] phone.connect: page budget elapsed for {} — giving up",
                        addr
                    );
                    let _ = event_tx.send(RuntimeEvent::Error(format!(
                        "phone.connect: no answer from {} (page timed out)",
                        addr
                    )));
                    manual_connect_pending = None;
                }
            }

            // ── phone.connect auth-budget watchdog ──────────────────
            // Authentication/encryption on the link we paged never
            // resolved (controller swallowed the event, phone went
            // silent mid-LMP). A paged-but-unsecured ACL is a dead
            // half-link — tear it down so either side can reconnect
            // cleanly.
            if let Some(auth) = &manual_connect_auth {
                if auth.started_at.elapsed() >= MANUAL_CONNECT_AUTH_BUDGET {
                    eprintln!(
                        "[AokieRadio] phone.connect: auth budget elapsed for {} (handle {:#06x}) — disconnecting",
                        auth.address, auth.connection_handle
                    );
                    let _ = event_tx.send(RuntimeEvent::Error(format!(
                        "phone.connect: securing the link to {} timed out — try again",
                        auth.address
                    )));
                    let _ = transport
                        .write_command(&hci::disconnect_command(auth.connection_handle, 0x13));
                    manual_connect_auth = None;
                    outbound_connect_session = false;
                }
            }

            // ── MAS-stall recovery watchdog ──────────────────────────
            //
            // Two-stage recovery:
            //
            // STAGE 1 (35s of no inbound ACL with MAP work pending):
            //   force-DISC dlci 11 + dlci 4 and re-queue Subscribe.
            //   Pixel *should* reopen MAS via SDP+SABM and push a fresh
            //   NotificationRegistration on MNS; previous-session
            //   pending FetchMessage / SendReply ops stay in the queue
            //   and resume after the new MAS comes up.
            //
            // STAGE 2 (an additional 20s after stage-1 with still no
            //          inbound ACL):
            //   Pixel is also wedged at the L2CAP signalling layer
            //   (it didn't even ack our SDP ConnectionRequest), so
            //   force-DISC was never going to be enough. Send
            //   HCI_Disconnect on the ACL handle. The phone typically
            //   auto-reconnects from its side after a clean hangup;
            //   if it doesn't, a user-initiated re-pair restores us
            //   to a known-good state — strictly better than a
            //   deceptively-connected link silently dropping SMS.
            //
            // Reset condition for both stages: any inbound ACL byte
            // (raw or framed) refreshes `last_acl_inbound_at` and
            // clears `mas_recovery_attempted_at`, so a healthy link
            // is never mistaken for a wedged one — even when our
            // parser is mid-stream on a multi-fragment listing.
            let map_work_pending = active_map_op.is_some() || !pending_map_ops.is_empty();
            // Idle→active edge: a fresh request is going out, so the
            // pre-existing silent period before this moment isn't
            // relevant to "did Pixel respond to our work." Without
            // this reset, the watchdog fires instantly when a poll
            // queues after a normal quiet conversation.
            if map_work_pending && !prev_map_work_pending {
                last_acl_inbound_at = Instant::now();
            }
            prev_map_work_pending = map_work_pending;
            let acl_silent = last_acl_inbound_at.elapsed() >= MAS_STALL_THRESHOLD;
            // Per-op deadline (2026-07-13): ACL silence alone can't catch
            // an op wedged mid-OBEX on an otherwise-healthy link — the
            // AT+CIND? keepalive replies keep refreshing the inbound
            // stamp forever (live: a mangled listing response left
            // PollInbox in AwaitingFirstGet permanently, blocking every
            // future poll behind active_map_op). Healthy ops finish in
            // under a second; 30s of no completion = wedged.
            if active_map_op.is_some() && active_map_op_since.is_none() {
                active_map_op_since = Some(Instant::now());
            }
            if active_map_op.is_none() {
                active_map_op_since = None;
            }
            let op_overdue = active_map_op_since
                .is_some_and(|t| t.elapsed() >= MAP_OP_DEADLINE);
            if op_overdue {
                if let Some(op) = active_map_op.as_ref() {
                    eprintln!(
                        "[AokieRadio] MAP op {} exceeded the {:?} op deadline on a healthy ACL — forcing stall recovery",
                        op.log_summary(),
                        MAP_OP_DEADLINE
                    );
                }
            }

            // Drop the stage-1 timestamp as soon as Pixel responds to
            // anything — the recovery worked, no need to escalate.
            if mas_recovery_attempted_at.is_some()
                && last_acl_inbound_at.elapsed() < MAS_STALL_THRESHOLD
            {
                eprintln!(
                    "[AokieRadio] MAS stall recovery: inbound ACL resumed — \
                     clearing stage-2 escalation timer"
                );
                mas_recovery_attempted_at = None;
            }

            if active_acl_handle.is_some() && map_work_pending && (acl_silent || op_overdue) {
                if let Some(stage1_at) = mas_recovery_attempted_at {
                    // Stage 2: stage-1 fired but Pixel still hasn't
                    // responded. Escalate to ACL teardown.
                    if stage1_at.elapsed() >= MAS_STALL_ESCALATION {
                        if let Some(handle) = active_acl_handle {
                            let head = acl_buffer
                                .iter()
                                .take(32)
                                .map(|b| format!("{:02x}", b))
                                .collect::<Vec<_>>()
                                .join(" ");
                            eprintln!(
                                "[AokieRadio] MAS stall escalation — stage-1 \
                                 recovery {:?} ago produced no inbound ACL \
                                 (acl_buffer={}B unparsed, head=[{}]); \
                                 sending HCI_Disconnect on handle {:#06x}",
                                stage1_at.elapsed(),
                                acl_buffer.len(),
                                head,
                                handle
                            );
                            let cmd = hci::disconnect_command(handle, 0x13);
                            if let Err(e) = transport.write_command(&cmd) {
                                let _ = event_tx.send(RuntimeEvent::Error(format!(
                                    "MAS stall escalation: HCI_Disconnect \
                                     write: {}",
                                    e
                                )));
                            }
                            // User-facing hint. We previously fired a
                            // one-shot outbound page here too; in the wild
                            // it brings the ACL link back up but Pixel
                            // stays passive on profile setup, leaving the
                            // user staring at a "connected" status that
                            // can't actually receive SMS. Better to leave
                            // the link cleanly down so they know to tap
                            // (or rely on Pixel's own auto-reconnect, which
                            // sometimes kicks in seconds later).
                            eprintln!(
                                "[AokieRadio] >>> Link torn down to recover \
                                 from stuck MAP session. Tap your phone \
                                 once in Bluetooth settings to reconnect — \
                                 we won't auto-page because that's been \
                                 producing dead-air links. <<<"
                            );
                            // Don't reset last_acl_inbound_at yet — the
                            // DisconnectionComplete event will arrive
                            // shortly and route through the existing
                            // teardown path which handles everything.
                            // We do clear the recovery timer so we don't
                            // double-fire HCI_Disconnect every heartbeat
                            // until the controller tears the ACL down.
                            mas_recovery_attempted_at = None;
                            last_acl_inbound_at = Instant::now();
                        }
                    }
                } else if let Some(rfcomm_cid) = l2cap_state.find_open_rfcomm_cid() {
                    // Stage 1: first time the watchdog has tripped this
                    // stall. Force-DISC the MAS+MNS DLCIs and re-queue
                    // Subscribe; remember when we did this so stage 2
                    // can decide whether the recovery worked.
                    let head = acl_buffer
                        .iter()
                        .take(32)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");
                    eprintln!(
                        "[AokieRadio] MAS stall watchdog fired — no inbound ACL \
                         for {:?} with active/pending MAP work (acl_buffer={}B \
                         unparsed, head=[{}]); force-DISC dlci 11 + dlci 4 and \
                         re-queueing Subscribe",
                        last_acl_inbound_at.elapsed(),
                        acl_buffer.len(),
                        head
                    );
                    match l2cap_state.rfcomm_force_disc_dlci(rfcomm_cid, 11) {
                        Ok(packet) => {
                            if let Err(e) = transport.write_acl(&packet) {
                                let _ = event_tx.send(RuntimeEvent::Error(format!(
                                    "MAS stall recovery: dlci 11 DISC write: {}",
                                    e
                                )));
                            }
                        }
                        Err(e) => {
                            eprintln!("[AokieRadio] MAS stall recovery: dlci 11 DISC build: {}", e)
                        }
                    }
                    // Only DISC the MNS dlci when MNS actually connected
                    // this session — on initiator-mux sessions it never
                    // does, and DISCing a never-open dlci just provokes
                    // a DM from the phone (harmless now that unknown-dlci
                    // DMs are ignored, but pointless traffic).
                    let mns_ever_up = mns_server
                        .lock()
                        .map(|g| !matches!(*g.state(), MnsState::AwaitingConnect))
                        .unwrap_or(false);
                    if mns_ever_up {
                        match l2cap_state.rfcomm_force_disc_dlci(rfcomm_cid, 4) {
                            Ok(packet) => {
                                if let Err(e) = transport.write_acl(&packet) {
                                    let _ = event_tx.send(RuntimeEvent::Error(format!(
                                        "MAS stall recovery: dlci 4 DISC write: {}",
                                        e
                                    )));
                                }
                            }
                            Err(e) => {
                                eprintln!("[AokieRadio] MAS stall recovery: dlci 4 DISC build: {}", e)
                            }
                        }
                    }
                    map_runtime = None;
                    // The ACTIVE op dies with the runtime — but a fresh
                    // SendReply/FetchMessage must be RE-QUEUED, not
                    // dropped (live 2026-07-13: the first kickoff SMS
                    // was silently lost exactly here). Stale SendReplys
                    // surface as SmsSendFailed so nothing dies quietly.
                    if let Some(op) = active_map_op.take() {
                        match &op {
                            PendingMapOp::SendReply {
                                queued_at,
                                recipient_phone,
                                ..
                            } => {
                                if queued_at.elapsed() < SEND_REPLY_RETAIN_TTL {
                                    eprintln!(
                                        "[AokieRadio] MAS stall recovery: re-queueing the in-flight SendReply (queued {:.1}s ago)",
                                        queued_at.elapsed().as_secs_f32()
                                    );
                                    pending_map_ops.push_back(op);
                                } else {
                                    let _ = event_tx.send(RuntimeEvent::SmsSendFailed {
                                        recipient_phone: recipient_phone.clone(),
                                        reason: format!(
                                            "abandoned after {:.0}s of MAS-stall recovery cycles",
                                            queued_at.elapsed().as_secs_f32()
                                        ),
                                    });
                                }
                            }
                            PendingMapOp::FetchMessage { queued_at, .. } => {
                                if queued_at.elapsed() < SEND_REPLY_RETAIN_TTL {
                                    pending_map_ops.push_back(op);
                                }
                            }
                            _ => {}
                        }
                    }
                    map_idle_since = None;
                    if let Ok(mut g) = mns_server.lock() {
                        g.reset();
                    }
                    if !matches!(pending_map_ops.front(), Some(PendingMapOp::Subscribe)) {
                        pending_map_ops.push_front(PendingMapOp::Subscribe);
                    }
                    // Stamp the recovery moment so the stage-2 branch
                    // above can fire if Pixel doesn't respond. Don't
                    // reset last_acl_inbound_at — leaving it stale is
                    // exactly how we get the stage-2 escalation.
                    mas_recovery_attempted_at = Some(Instant::now());
                } else {
                    // No open RFCOMM mux means the link itself is
                    // probably dropping; resetting the watchdog avoids
                    // a tight retry loop while the ACL gets torn down.
                    eprintln!(
                        "[AokieRadio] MAS stall detected but no open RFCOMM cid — \
                         skipping recovery, resetting watchdog"
                    );
                    last_acl_inbound_at = Instant::now();
                    mas_recovery_attempted_at = None;
                }
            }
        }
        // 4a) Drain control commands first so a shutdown / answer
        //     queued during the previous tick doesn't sit waiting on
        //     the next read timeout.
        loop {
            match control_rx.try_recv() {
                Ok(ControlCommand::Shutdown) => return Ok(()),
                Ok(ControlCommand::Answer) => {
                    let packets =
                        l2cap_state.build_hfp_call_control_packets(HfpAtCommand::Answer)?;
                    eprintln!(
                        "[AokieRadio] Answer requested — built {} ACL packet(s) for ATA",
                        packets.len()
                    );
                    for packet in &packets {
                        if let Err(e) = transport.write_acl(packet) {
                            let _ = event_tx.send(RuntimeEvent::Error(format!("answer: {}", e)));
                        }
                    }
                    if !packets.is_empty() {
                        answer_sent_for_call = true;
                    }
                }
                Ok(ControlCommand::RejectOrHangup) => {
                    let packets =
                        l2cap_state.build_hfp_call_control_packets(HfpAtCommand::RejectOrHangup)?;
                    eprintln!(
                        "[AokieRadio] Hangup requested — built {} ACL packet(s) for AT+CHUP",
                        packets.len()
                    );
                    for packet in &packets {
                        if let Err(e) = transport.write_acl(packet) {
                            let _ = event_tx.send(RuntimeEvent::Error(format!("hangup: {}", e)));
                        }
                    }
                }
                Ok(ControlCommand::Dial(number)) => {
                    let packets = l2cap_state
                        .build_hfp_call_control_packets(HfpAtCommand::Dial(number.clone()))?;
                    eprintln!(
                        "[AokieRadio] Dial requested — built {} ACL packet(s) for ATD",
                        packets.len()
                    );
                    if packets.is_empty() {
                        // No open HFP SLC = no way to place the call. Surface
                        // it — a silently-swallowed dial reads as "no answer"
                        // to the caller-side watchdog for no reason it can see.
                        let _ = event_tx.send(RuntimeEvent::Error(
                            "dial: no open HFP service-level connection".to_string(),
                        ));
                    }
                    for packet in &packets {
                        if let Err(e) = transport.write_acl(packet) {
                            let _ = event_tx.send(RuntimeEvent::Error(format!("dial: {}", e)));
                        }
                    }
                }
                Ok(ControlCommand::HoldSwap) => {
                    let packets = l2cap_state.build_hfp_call_control_packets(
                        HfpAtCommand::CallHold(ChldAction::HoldActiveAcceptOther),
                    )?;
                    eprintln!(
                        "[AokieRadio] HoldSwap requested — built {} ACL packet(s) for AT+CHLD=2",
                        packets.len()
                    );
                    if packets.is_empty() {
                        // Same policy as Dial: a silently-swallowed switch
                        // would leave the switchboard believing a transition
                        // is in flight that never touched the wire.
                        let _ = event_tx.send(RuntimeEvent::Error(
                            "holdSwap: no open HFP service-level connection".to_string(),
                        ));
                    }
                    for packet in &packets {
                        if let Err(e) = transport.write_acl(packet) {
                            let _ =
                                event_tx.send(RuntimeEvent::Error(format!("holdSwap: {}", e)));
                        }
                    }
                }
                Ok(ControlCommand::SendAudio(samples)) => {
                    // No active SCO = no call that can hear this. Refuse the
                    // queue instead of buffering: audio synthesized for a call
                    // whose link just died would otherwise sit in the TX queue
                    // and PLAY INTO THE NEXT CALL's fresh SCO (observed live
                    // 2026-07-13: a new call opened with the tail of the
                    // previous call's cut-off reply). Every legitimate speaker
                    // (greeting, agent replies, operatorSpeak) already gates
                    // on sample_rate > 0, so pre-SCO queuing is never wanted.
                    if active_sco_handle.is_none() {
                        eprintln!(
                            "[AokieRadio] SCO TX: DROPPED {} samples — no active SCO link (stale call audio must not leak into the next call)",
                            samples.len()
                        );
                        continue;
                    }
                    // Audio level on the way in. If the caller hears
                    // silence and this is also silence, the upstream
                    // TTS is producing zeros; if it has level here but
                    // silence on the air, the codec/transport leg is
                    // broken.
                    let (peak, rms) = if samples.is_empty() {
                        (0i32, 0.0f64)
                    } else {
                        let mut peak: i32 = 0;
                        let mut sum_sq: u128 = 0;
                        for &s in &samples {
                            let mag = (s as i32).abs();
                            if mag > peak {
                                peak = mag;
                            }
                            sum_sq += (mag as u128) * (mag as u128);
                        }
                        let mean_sq = (sum_sq / samples.len() as u128) as f64;
                        (peak, mean_sq.sqrt())
                    };
                    let accepted = sco_tx_queue.push_samples(&samples);
                    eprintln!(
                        "[AokieRadio] SCO TX: queued {}/{} samples \
                         (peak={}, rms={:.0}; queue depth {}, dropped total {})",
                        accepted,
                        samples.len(),
                        peak,
                        rms,
                        sco_tx_queue.len(),
                        sco_tx_queue.dropped_samples(),
                    );
                }
                Ok(ControlCommand::FlushTxAudio) => {
                    let dropped = sco_tx_queue.len();
                    sco_tx_queue.clear();
                    eprintln!(
                        "[AokieRadio] SCO TX flush requested — dropped {} queued samples",
                        dropped
                    );
                }
                Ok(ControlCommand::SendSms {
                    recipient_phone,
                    body,
                    msg_type,
                }) => {
                    // Echo the inbound msg_type so the reply lands in
                    // the same thread on the sender's phone. Treat
                    // ASCII-case-insensitive "MMS" as the trigger;
                    // anything else (SMS_GSM / SMS_CDMA / unknown /
                    // None for manual sends) goes via the legacy SMS
                    // bMessage builder.
                    let is_mms = msg_type
                        .as_deref()
                        .map(|t| t.eq_ignore_ascii_case("MMS"))
                        .unwrap_or(false);
                    let bmessage = if is_mms {
                        bmessage::build_mms_push(&recipient_phone, &body)
                    } else {
                        bmessage::build_sms_push(&recipient_phone, &body)
                    };
                    pending_map_ops.push_back(PendingMapOp::SendReply {
                        bmessage,
                        recipient_phone,
                        queued_at: Instant::now(),
                    });
                    eprintln!(
                        "[AokieRadio] SendSms enqueued ({} pending MAP ops, type={})",
                        pending_map_ops.len(),
                        if is_mms { "MMS" } else { "SMS_GSM" }
                    );
                }
                Ok(ControlCommand::RemovePaired { address, reply }) => {
                    // PAIR-001: forget = DISCONNECT FIRST, then drop the credential.
                    // Without the disconnect, an already-connected phone keeps its
                    // live (encrypted) session after the operator revoked it, and
                    // "forget" only takes effect at some future reconnect.
                    let remote_is_target = status
                        .addresses
                        .read()
                        .map(|a| a.remote.eq_ignore_ascii_case(&address))
                        .unwrap_or(false);
                    if remote_is_target {
                        if let Some(handle) = active_acl_handle {
                            // 0x13 = Remote User Terminated Connection.
                            let cmd = hci::disconnect_command(handle, 0x13);
                            match transport.write_command(&cmd) {
                                Ok(()) => eprintln!(
                                    "[AokieRadio] forget {}: disconnecting active ACL \
                                     (handle {:#06x}) before removing its link key",
                                    address, handle
                                ),
                                Err(e) => eprintln!(
                                    "[AokieRadio] forget {}: ACL disconnect write failed \
                                     ({}) — removing the link key anyway",
                                    address, e
                                ),
                            }
                        }
                    }
                    // AOK-BT-001: forget a bonded device on its owner thread, then
                    // refresh the snapshot so listPaired reflects it immediately.
                    let result = pairing_store.remove(&address);
                    if matches!(result, Ok(true)) {
                        if let Ok(mut snap) = status.bonded_devices.write() {
                            *snap = pairing_store.list_devices();
                        }
                        eprintln!("[AokieRadio] removed bonded device {address}");
                    }
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::Disconnect { address, reply }) => {
                    // Disconnect the live link WITHOUT dropping the bond. Only
                    // acts when the target is the currently-connected phone
                    // (the runtime tracks a single active ACL). The phone,
                    // for which we stay connectable, typically re-pages us and
                    // reconnects — so this is the remote "reconnect if wedged".
                    let remote_is_target = status
                        .addresses
                        .read()
                        .map(|a| a.remote.eq_ignore_ascii_case(&address))
                        .unwrap_or(false);
                    let result = if remote_is_target {
                        if let Some(handle) = active_acl_handle {
                            let cmd = hci::disconnect_command(handle, 0x13);
                            match transport.write_command(&cmd) {
                                Ok(()) => {
                                    eprintln!(
                                        "[AokieRadio] disconnect {}: tore down active ACL \
                                         (handle {:#06x}); bond kept — the phone can reconnect",
                                        address, handle
                                    );
                                    Ok(true)
                                }
                                Err(e) => Err(format!("ACL disconnect write failed: {e}")),
                            }
                        } else {
                            // Connected per status but no handle tracked — nothing to cut.
                            Ok(false)
                        }
                    } else {
                        // Not the connected phone (or nothing connected): a no-op,
                        // not an error — the UI shows it as already disconnected.
                        Ok(false)
                    };
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::Connect { address, reply }) => {
                    // HARD-001 outbound reconnect: page the bonded phone; the
                    // ConnectionComplete arm below starts the SDP/RFCOMM/SLC
                    // driving. Truthful accepted-only semantics — the
                    // phone.connected event is the outcome.
                    let result = if !pairing_store.contains(&address) {
                        Err(format!(
                            "{} is not a paired phone — pair it from the phone's Bluetooth settings first",
                            address
                        ))
                    } else if active_acl_handle.is_some() {
                        let remote_is_target = status
                            .addresses
                            .read()
                            .map(|a| a.remote.eq_ignore_ascii_case(&address))
                            .unwrap_or(false);
                        if remote_is_target {
                            Ok(false) // already connected — no-op, not an error
                        } else {
                            Err("another phone currently holds the link — disconnect it first"
                                .to_string())
                        }
                    } else if manual_connect_pending.is_some() {
                        Err("a reconnect attempt is already in progress".to_string())
                    } else {
                        match hci::create_connection_command_default(&address) {
                            Ok(cmd) => match transport.write_command(&cmd) {
                                Ok(()) => {
                                    eprintln!(
                                        "[AokieRadio] phone.connect: paging bonded device {} (HCI Create_Connection)",
                                        address
                                    );
                                    manual_connect_pending =
                                        Some((address.clone(), Instant::now()));
                                    Ok(true)
                                }
                                Err(e) => Err(format!("HCI Create_Connection write failed: {e}")),
                            },
                            Err(e) => Err(format!("bad BD_ADDR: {e}")),
                        }
                    };
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::ConfirmPairing {
                    address,
                    accept,
                    reply,
                }) => {
                    // PAIR-001: resolve the held SSP numeric comparison. Only the
                    // exact pending address can be answered — anything else is a
                    // typed error so the UI can't confirm a stale/different device.
                    let result = match &pending_ssp_confirm {
                        Some((pending_addr, _)) if pending_addr.eq_ignore_ascii_case(&address) => {
                            let command = if accept {
                                hci::user_confirmation_request_reply_command(pending_addr)
                            } else {
                                hci::user_confirmation_request_negative_reply_command(pending_addr)
                            };
                            command.and_then(|cmd| transport.write_command(&cmd)).map(|()| {
                                eprintln!(
                                    "[AokieRadio] operator {} SSP numeric comparison for {}",
                                    if accept { "CONFIRMED" } else { "REJECTED" },
                                    pending_addr
                                );
                            })
                        }
                        Some((pending_addr, _)) => Err(format!(
                            "pending pairing confirmation is for {}, not {}",
                            pending_addr, address
                        )),
                        None => Err("no pairing confirmation is pending".to_string()),
                    };
                    if result.is_ok() {
                        pending_ssp_confirm = None;
                        status.pairing_confirm.clear();
                    }
                    let _ = reply.send(result);
                }
                Err(stdmpsc::TryRecvError::Empty) => break,
                Err(stdmpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // PAIR-001: a held SSP confirmation the operator never answered is refused
        // on its 25s deadline. Window state is DELIBERATELY not part of this: the
        // ACL for a fresh pairing comes up (so status shows "connected") a beat
        // before the numeric-comparison request, and the Desktop UI can auto-close
        // the window in that gap — but once a code has been shown to the operator,
        // only their answer, the deadline, or the peer walking away
        // (SimplePairingComplete / DisconnectionComplete, handled below) may resolve
        // it. Tying it to the window turned every real pairing into an instant
        // "refused — pairing window closed".
        if let Some((pending_addr, deadline)) = &pending_ssp_confirm {
            if Instant::now() >= *deadline {
                match hci::user_confirmation_request_negative_reply_command(pending_addr)
                    .and_then(|cmd| transport.write_command(&cmd))
                {
                    Ok(()) => eprintln!(
                        "[AokieRadio] refused SSP numeric comparison for {} — operator did not confirm in time",
                        pending_addr
                    ),
                    Err(e) => eprintln!(
                        "[AokieRadio] SSP negative reply for {} failed ({}) — \
                         the LMP timeout will abort the handshake",
                        pending_addr, e
                    ),
                }
                pending_ssp_confirm = None;
                status.pairing_confirm.clear();
            }
        }

        // AOK-BT-001: reconcile the controller's scan-enable with the pairing
        // window — this one place handles explicit open/close AND timeout expiry.
        // Discoverable+connectable while the window is open, connectable-only at rest.
        let want_discoverable = status.pairing_window.is_open();
        if want_discoverable != scan_discoverable {
            let scan_enable = if want_discoverable {
                manager::AOKIE_SCAN_ENABLE_CONNECTABLE_DISCOVERABLE
            } else {
                manager::AOKIE_SCAN_ENABLE_CONNECTABLE
            };
            match manager::write_scan_enable(&transport, scan_enable) {
                Ok(()) => {
                    scan_discoverable = want_discoverable;
                    eprintln!(
                        "[AokieRadio] pairing window {} — scan_enable=0x{:02x}",
                        if want_discoverable { "OPEN (discoverable)" } else { "closed (connectable-only)" },
                        scan_enable
                    );
                }
                Err(e) => eprintln!("[AokieRadio] scan-enable reconcile failed: {e} — retrying next tick"),
            }
        }

        // 4b) HCI events (interrupt endpoint).
        match transport.read_event() {
            Ok(packet) => {
                let event = match hci::parse_typed_event(&packet) {
                    Ok(event) => event,
                    Err(e) => {
                        // A truncated / malformed event from the
                        // controller (occasional USB short-packet hiccup)
                        // must not kill an active call. Log and skip;
                        // the next event read picks up real traffic.
                        eprintln!(
                            "[AokieRadio] HCI event parse error: {} — skipping ({} bytes, head {:02x?})",
                            e,
                            packet.len(),
                            &packet[..packet.len().min(16)],
                        );
                        continue;
                    }
                };
                let bonds_before = pairing_store.len();
                match manager::handle_hci_event_with_policy(
                    &transport,
                    &mut pairing_store,
                    &event,
                    selected_codec.as_ref(),
                    // AOK-BT-001 + PAIR-001: a fresh bond / unknown-device ACL is
                    // admitted only while the window is open; SSP runs as numeric
                    // comparison held for the operator; legacy PIN only via the
                    // explicit compat setting.
                    manager::PairingPolicy::runtime(
                        status.pairing_window.is_open(),
                        legacy_pin_allowed,
                    ),
                ) {
                    Ok(Some(manager::PairingOutcome::Logged(action))) => {
                        // Surface ACL/SCO ConnectionRequest acceptance — without
                        // this, an SCO accept that the controller silently fails
                        // to honor looks identical to "phone never asked for
                        // SCO" in the log. Now we can tell them apart.
                        eprintln!("[AokieRadio] HCI {}", action);
                    }
                    Ok(Some(manager::PairingOutcome::ConfirmationPending {
                        address,
                        numeric_value,
                    })) => {
                        // PAIR-001: the reply is HELD — surface the code to the
                        // operator and start the confirmation deadline.
                        let deadline =
                            Instant::now() + Duration::from_secs(PAIRING_CONFIRM_TIMEOUT_SECS);
                        pending_ssp_confirm = Some((address.clone(), deadline));
                        status.pairing_confirm.set(PendingPairingConfirm {
                            address: address.clone(),
                            numeric_value,
                            expires_epoch_secs: now_epoch_secs()
                                .saturating_add(PAIRING_CONFIRM_TIMEOUT_SECS),
                        });
                        eprintln!(
                            "[AokieRadio] SSP numeric comparison for {} — code {:06} awaiting \
                             operator confirmation ({}s)",
                            address, numeric_value, PAIRING_CONFIRM_TIMEOUT_SECS
                        );
                        let _ = event_tx.send(RuntimeEvent::PairingConfirmRequired {
                            address,
                            numeric_value,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("[AokieRadio] HCI event handler error: {} — continuing", e);
                        // A transient pipe-read timeout inside a
                        // command-status wait (Win32 121 under heavy
                        // event traffic, e.g. during a connect) is NOT a
                        // hardware issue — the command almost always
                        // landed and everything proceeds. Surfacing it
                        // as RuntimeEvent::Error raised a scary
                        // "hardware issue" toast and left health
                        // degraded until restart (live report
                        // 2026-07-13). Real failures still surface.
                        if !manager::is_timeout_error(&e) {
                            let _ =
                                event_tx.send(RuntimeEvent::Error(format!("hci event: {}", e)));
                        }
                        continue;
                    }
                }
                // PAIR-001: a peer that walks away mid-confirmation (SSP completes
                // negatively or the ACL drops) clears the held prompt — no stale
                // "confirm code" UI for a phone that is no longer pairing.
                if pending_ssp_confirm.is_some()
                    && matches!(
                        &event,
                        hci::HciEvent::SimplePairingComplete { .. }
                            | hci::HciEvent::DisconnectionComplete { .. }
                    )
                {
                    pending_ssp_confirm = None;
                    status.pairing_confirm.clear();
                }
                // A new bond just landed (AOK-BT-001): refresh the snapshot and
                // auto-close the pairing window — one device per window, so a
                // successful pair doesn't leave us discoverable for the full timeout.
                if pairing_store.len() != bonds_before {
                    if let Ok(mut snap) = status.bonded_devices.write() {
                        *snap = pairing_store.list_devices();
                    }
                    if pairing_store.len() > bonds_before {
                        status.pairing_window.close();
                    }
                }
                forward_hci_event(&event, active_sco_handle, &event_tx, &status);
                if let hci::HciEvent::SynchronousConnectionComplete {
                    status: scstatus,
                    connection_handle,
                    link_type,
                    tx_packet_length,
                    rx_packet_length,
                    air_mode,
                    transmission_interval,
                    retransmission_window,
                    ..
                } = &event
                {
                    if *scstatus == 0 {
                        active_sco_handle = Some(*connection_handle);
                        active_sco_tx_packet_len = Some(*tx_packet_length as usize);
                        last_sco_rx_bytes_at = None;
                        sco_rx_silence_logged_at = None;
                        // Every call's audio starts from an EMPTY queue —
                        // belt-and-braces over the no-SCO SendAudio refusal
                        // and the drop-time clear: whatever anyone queued
                        // between links (a reply cut off by a dead link, a
                        // race with teardown), the next caller must never
                        // hear the previous call's leftovers.
                        if !sco_tx_queue.is_empty() {
                            eprintln!(
                                "[AokieRadio] SCO establish: discarding {} stale queued TX samples from a previous call",
                                sco_tx_queue.len()
                            );
                            sco_tx_queue.clear();
                        }
                        // Mirror BTStack's `usb_sco_start`: the SetCurrentAlternateSetting
                        // + RegisterIsochBuffer + IN-ring bootstrap fire here, AFTER
                        // SynchronousConnectionComplete delivers a valid
                        // connection_handle. The previous "Stagger 2" pre-arm on
                        // ConnectionRequest was unverified theory and contradicts the
                        // BTStack reference (`hci.c`'s `set_sco_config` only fires
                        // from `SynchronousConnectionComplete`). On Broadcom 21ec the
                        // pre-arm produced zero-byte iso URBs while the BTStack-bridge
                        // path on the same dongle carried bytes both ways.
                        let params =
                            manager::sco_accept_parameters(*link_type, selected_codec.as_ref());
                        let alt_result =
                            transport.configure_sco_alt_setting(params.voice_setting, 1);
                        status.call_active.store(true, Ordering::Relaxed);
                        let (codec, sample_rate) = selected_codec
                            .clone()
                            .unwrap_or_else(|| ("CVSD".to_string(), 8000));
                        status.sample_rate.store(sample_rate, Ordering::Relaxed);
                        let cfg = transport.sco_transport_config();
                        // air_mode is the controller's authoritative answer
                        // about how this SCO link carries audio:
                        //   0x00 µ-law, 0x01 A-law, 0x02 CVSD, 0x03 transparent
                        // For mSBC at WBS we must see 0x03; anything else
                        // means our Write_Voice_Setting(0x0043) didn't
                        // stick and the controller is silently re-encoding
                        // / decoding our mSBC bytes as PCM. That alone
                        // would explain a SCO RX path that never carries
                        // mic audio: the controller thinks it's CVSD and
                        // routes it via PCM I/O instead of USB iso.
                        let air_mode_label = match *air_mode {
                            0x00 => "µ-law",
                            0x01 => "A-law",
                            0x02 => "CVSD",
                            0x03 => "transparent",
                            _ => "unknown",
                        };
                        eprintln!(
                            "[AokieRadio] SCO link up handle {:#06x} link_type={} codec={} rate={}Hz tx_len={} rx_len={} air_mode=0x{:02x} ({}) tx_interval={} retx_window={} alt_setting={} cfg={:?}",
                            connection_handle,
                            link_type,
                            codec,
                            sample_rate,
                            tx_packet_length,
                            rx_packet_length,
                            air_mode,
                            air_mode_label,
                            transmission_interval,
                            retransmission_window,
                            match &alt_result {
                                Ok(()) => "ok".to_string(),
                                Err(e) => format!("err: {}", e),
                            },
                            cfg,
                        );
                        // Don't propagate alt-setting failure as fatal:
                        // the SCO link is already up at the HCI layer
                        // and tearing the runtime down here would
                        // disconnect HFP/MAP/PBAP too. If the iso pipes
                        // never armed, audio will be silent (we already
                        // hit that under the WinUSB-bound stack across
                        // vendors), but the user can still hang up the
                        // call cleanly.
                        if let Err(e) = &alt_result {
                            eprintln!(
                                "[AokieRadio] SCO alt-setting configure failed: {} — \
                                 audio will be silent for this call but link stays up",
                                e
                            );
                        }
                        let _ = event_tx.send(RuntimeEvent::AudioConnected { codec, sample_rate, armed: alt_result.is_ok() });
                    } else {
                        eprintln!(
                            "[AokieRadio] SCO link FAILED status 0x{:02x} handle {:#06x}",
                            scstatus, connection_handle,
                        );
                    }
                }
                // Device name/model capture: the phone answered our
                // Remote Name Request (issued on ConnectionComplete). Persist
                // it against the bond so the "paired phones" list shows the
                // model, refresh the snapshot, and mirror it live for the
                // connected phone. status != 0 (name unavailable) is ignored.
                if let hci::HciEvent::RemoteNameRequestComplete {
                    status: nstatus,
                    address,
                    name,
                } = &event
                {
                    let name = name.trim();
                    if *nstatus == 0 && !name.is_empty() {
                        eprintln!("[AokieRadio] remote name for {address}: {name:?}");
                        // Live name for the CURRENTLY connected phone.
                        if let Ok(mut a) = status.addresses.write() {
                            if a.remote.eq_ignore_ascii_case(address) {
                                a.remote_name = Some(name.to_string());
                            }
                        }
                        // Persist against the bond (if bonded) + refresh snapshot.
                        match pairing_store.set_name(address, name) {
                            Ok(true) => {
                                if let Ok(mut snap) = status.bonded_devices.write() {
                                    *snap = pairing_store.list_devices();
                                }
                            }
                            Ok(false) => {}
                            Err(e) => eprintln!(
                                "[AokieRadio] failed to persist device name for {address}: {e}"
                            ),
                        }
                    }
                }
                // Phase 3e: track the ACL handle for PbapRuntime. We
                // set it here (not inside forward_hci_event) because
                // forward_hci_event has no access to the IO-loop
                // locals. Only ACL ConnectionComplete with status=0
                // counts; SCO and failed connects are filtered out.
                if let hci::HciEvent::ConnectionComplete {
                    status: cstatus,
                    connection_handle,
                    link_type,
                    address,
                    ..
                } = &event
                {
                    if *cstatus == 0 && *link_type == hci::LINK_TYPE_ACL {
                        active_acl_handle = Some(*connection_handle);
                        // Ask the phone for its friendly name/model so Device
                        // Setup can show "Lance's Pixel 8" instead of a bare
                        // MAC (disambiguates multiple bonded phones). Best
                        // effort — a failure just leaves the address showing.
                        // Clear any stale name from a previous peer first.
                        if let Ok(mut a) = status.addresses.write() {
                            a.remote_name = None;
                        }
                        match hci::remote_name_request_command(address) {
                            Ok(cmd) => {
                                if let Err(e) = transport.write_command(&cmd) {
                                    eprintln!(
                                        "[AokieRadio] remote name request write failed for {address}: {e}"
                                    );
                                }
                            }
                            Err(e) => eprintln!(
                                "[AokieRadio] remote name request build failed for {address}: {e}"
                            ),
                        }
                    }
                    // HARD-001 phone.connect: the page WE issued answered.
                    // Before ANY profile traffic we must drive LMP
                    // authentication + encryption ourselves — inbound, the
                    // phone (as paging master) does this, and skipping it
                    // outbound makes the phone tear the link down with 0x05
                    // at our first RFCOMM ConnectionRequest. SDP → RFCOMM →
                    // SLC starts from the Encryption_Change arm below. No
                    // role switch: we stay master (standard for the paging
                    // HF; the phone may request its own switch via LMP).
                    if let Some((pending_addr, _)) = &manual_connect_pending {
                        if pending_addr.eq_ignore_ascii_case(address)
                            && *link_type == hci::LINK_TYPE_ACL
                        {
                            if *cstatus == 0 {
                                eprintln!(
                                    "[AokieRadio] phone.connect: {} answered our page (handle {:#06x}) — authenticating the link",
                                    address, connection_handle
                                );
                                outbound_connect_session = true;
                                // MAP/MNS now ride secondary client DLCIs on the
                                // HFP client's own multiplexer (2026-07-13:
                                // RfcommClientState::attach_client_dlci), so the
                                // SLC-ready MAP subscribe runs on outbound
                                // sessions too — SMS send/receive no longer
                                // waits for a phone-initiated reconnect.
                                let cmd =
                                    hci::authentication_requested_command(*connection_handle);
                                match transport.write_command(&cmd) {
                                    Ok(()) => {
                                        manual_connect_auth = Some(ManualConnectAuth {
                                            connection_handle: *connection_handle,
                                            address: address.clone(),
                                            awaiting_encryption: false,
                                            started_at: Instant::now(),
                                        });
                                    }
                                    Err(e) => {
                                        let _ = event_tx.send(RuntimeEvent::Error(format!(
                                            "phone.connect: Authentication_Requested write failed: {}",
                                            e
                                        )));
                                        let _ = transport.write_command(
                                            &hci::disconnect_command(*connection_handle, 0x13),
                                        );
                                        outbound_connect_session = false;
                                    }
                                }
                            } else {
                                eprintln!(
                                    "[AokieRadio] phone.connect: page to {} failed status 0x{:02x}",
                                    address, cstatus
                                );
                                let _ = event_tx.send(RuntimeEvent::Error(format!(
                                    "phone.connect: {} did not answer the page (status 0x{:02x}) — make sure the phone is nearby with Bluetooth on",
                                    address, cstatus
                                )));
                            }
                            manual_connect_pending = None;
                        }
                    }
                    // Clear auto-reconnect pending state regardless of
                    // success/failure — the page either landed (and we
                    // now have an ACL we want to keep) or it didn't (and
                    // we'll try the next addr after AUTO_RECONNECT_INTERVAL).
                    if let Some(pending_addr) = auto_reconnect_pending_addr.take() {
                        if pending_addr.eq_ignore_ascii_case(address)
                            && *link_type == hci::LINK_TYPE_ACL
                        {
                            if *cstatus == 0 {
                                eprintln!(
                                    "[AokieRadio] auto-reconnect: paired device {} \
                                     answered our page (handle {:#06x})",
                                    address, connection_handle
                                );
                                // We just paged outbound, which means we're the
                                // ACL master. Bluedroid PSEs (Pixel) only auto-
                                // initiate profiles when the AG is master, so
                                // ask the controller to swap us to slave. The
                                // peer accepted "Allow Role Switch" in our
                                // Create_Connection so this should succeed
                                // without bothering the user. If it fails the
                                // link keeps working — phone just won't
                                // auto-page profiles, which is the same
                                // dead-end we saw before this fix.
                                match hci::switch_role_command(address, 0x01) {
                                    Ok(cmd) => match transport.write_command(&cmd) {
                                        Ok(()) => eprintln!(
                                            "[AokieRadio] auto-reconnect: \
                                             requesting role switch to slave on {} \
                                             so AG drives profile setup",
                                            address
                                        ),
                                        Err(e) => eprintln!(
                                            "[AokieRadio] auto-reconnect: \
                                             Switch_Role write failed for {}: {}",
                                            address, e
                                        ),
                                    },
                                    Err(e) => eprintln!(
                                        "[AokieRadio] auto-reconnect: bad BD_ADDR \
                                         in Switch_Role for {}: {}",
                                        address, e
                                    ),
                                }
                            } else {
                                eprintln!(
                                    "[AokieRadio] auto-reconnect: page to {} failed \
                                     status 0x{:02x} — will try next addr after {:?}",
                                    address, cstatus, AUTO_RECONNECT_INTERVAL
                                );
                            }
                            auto_reconnect_pending_since = None;
                            auto_reconnect_next_attempt =
                                Some(Instant::now() + AUTO_RECONNECT_INTERVAL);
                        } else {
                            // The Connection Complete was for a different
                            // address (e.g. the phone paged us first while
                            // our outbound page to a stale addr was in
                            // flight). Keep the pending state — the
                            // watchdog will time it out cleanly.
                            auto_reconnect_pending_addr = Some(pending_addr);
                        }
                    }
                }
                // HARD-001 phone.connect step 2: authentication resolved.
                // Success → ask for link encryption; failure → the phone
                // most likely deleted the bond (or the stored key is
                // stale), so tear down truthfully and tell the user to
                // re-pair rather than leaving a dead half-link up.
                if let hci::HciEvent::AuthenticationComplete {
                    status: astatus,
                    connection_handle,
                } = &event
                {
                    let matches_auth = manual_connect_auth.as_ref().is_some_and(|a| {
                        a.connection_handle == *connection_handle && !a.awaiting_encryption
                    });
                    if matches_auth {
                        if *astatus == 0 {
                            let auth = manual_connect_auth.as_mut().expect("checked above");
                            eprintln!(
                                "[AokieRadio] phone.connect: {} authenticated — enabling link encryption",
                                auth.address
                            );
                            let cmd = hci::set_connection_encryption_command(
                                *connection_handle,
                                true,
                            );
                            match transport.write_command(&cmd) {
                                Ok(()) => auth.awaiting_encryption = true,
                                Err(e) => {
                                    let _ = event_tx.send(RuntimeEvent::Error(format!(
                                        "phone.connect: Set_Connection_Encryption write failed: {}",
                                        e
                                    )));
                                    let _ = transport.write_command(
                                        &hci::disconnect_command(*connection_handle, 0x13),
                                    );
                                    manual_connect_auth = None;
                                    outbound_connect_session = false;
                                }
                            }
                        } else {
                            let auth = manual_connect_auth.take().expect("checked above");
                            eprintln!(
                                "[AokieRadio] phone.connect: authentication with {} FAILED status 0x{:02x}",
                                auth.address, astatus
                            );
                            let _ = event_tx.send(RuntimeEvent::Error(format!(
                                "phone.connect: {} rejected our stored pairing (authentication failure 0x{:02x}) — forget Aokie on the phone and pair again",
                                auth.address, astatus
                            )));
                            // 0x05 = Authentication Failure: the honest reason.
                            let _ = transport.write_command(&hci::disconnect_command(
                                *connection_handle,
                                0x05,
                            ));
                            outbound_connect_session = false;
                        }
                    }
                }
                // HARD-001 phone.connect step 3: the link is encrypted —
                // NOW start the profile driving (SDP → RFCOMM → SLC).
                if let hci::HciEvent::EncryptionChange {
                    status: estatus,
                    connection_handle,
                    encryption_enabled,
                } = &event
                {
                    let matches_auth = manual_connect_auth.as_ref().is_some_and(|a| {
                        a.connection_handle == *connection_handle && a.awaiting_encryption
                    });
                    if matches_auth {
                        let auth = manual_connect_auth.take().expect("checked above");
                        if *estatus == 0 && *encryption_enabled != 0 {
                            eprintln!(
                                "[AokieRadio] phone.connect: link to {} encrypted — driving HFP setup",
                                auth.address
                            );
                            let mut runtime = HfpConnectRuntime::new(
                                *connection_handle,
                                wbs_supported,
                                call_waiting_enabled,
                            );
                            match runtime.start(&mut l2cap_state) {
                                Ok(packets) => {
                                    let mut started = true;
                                    for packet in &packets {
                                        if let Err(e) = transport.write_acl(packet) {
                                            let _ = event_tx.send(RuntimeEvent::Error(
                                                format!("phone.connect SDP start: {}", e),
                                            ));
                                            started = false;
                                            break;
                                        }
                                    }
                                    if started {
                                        hfp_connect_runtime = Some(runtime);
                                    }
                                }
                                Err(e) => {
                                    let _ = event_tx.send(RuntimeEvent::Error(format!(
                                        "phone.connect: {}",
                                        e
                                    )));
                                }
                            }
                        } else {
                            let _ = event_tx.send(RuntimeEvent::Error(format!(
                                "phone.connect: encrypting the link to {} failed (status 0x{:02x}) — try again, or re-pair if it keeps failing",
                                auth.address, estatus
                            )));
                            let _ = transport.write_command(&hci::disconnect_command(
                                *connection_handle,
                                0x13,
                            ));
                            outbound_connect_session = false;
                        }
                    }
                }
                if let hci::HciEvent::DisconnectionComplete {
                    status: dstatus,
                    connection_handle,
                    reason,
                    ..
                } = &event
                {
                    if *dstatus == 0 {
                        l2cap_state.remove_connection(*connection_handle);
                        // Clear the ACL handle and tear down any
                        // in-flight PBAP fetch when the ACL link drops.
                        // SCO disconnects don't touch ACL state, so
                        // gate on "this is NOT the SCO handle".
                        if active_acl_handle == Some(*connection_handle)
                            && active_sco_handle != Some(*connection_handle)
                        {
                            active_acl_handle = None;
                            if pbap_runtime.is_some() {
                                eprintln!(
                                    "[AokieRadio] ACL handle {:#06x} dropped — discarding in-flight PBAP runtime",
                                    connection_handle
                                );
                            }
                            pbap_runtime = None;
                            // HARD-001: an in-flight outbound HFP connect dies
                            // with its ACL; the session flag resets so inbound
                            // reconnects get normal MAP/MNS behaviour again.
                            if hfp_connect_runtime.is_some() {
                                eprintln!(
                                    "[AokieRadio] ACL handle {:#06x} dropped — discarding in-flight HFP connect runtime",
                                    connection_handle
                                );
                            }
                            hfp_connect_runtime = None;
                            manual_connect_auth = None;
                            outbound_connect_session = false;
                            // Phase 4e: tear down MAP state too. We
                            // preserve fresh SendReply *and* FetchMessage
                            // ops (under SEND_REPLY_RETAIN_TTL) so the
                            // customer's pending reply and any in-flight
                            // body fetch automatically retry once a
                            // healthy ACL is back. Drop Subscribe and
                            // PollInbox: the SLC handler re-pushes
                            // Subscribe automatically and PollInbox runs
                            // on its own timer.
                            //
                            // Pixel/Bluedroid uses persistent message
                            // handles per phone, so a FetchMessage queued
                            // in the prior session is still valid against
                            // the new MAS session.
                            let pre_count = pending_map_ops.len();
                            // The ACTIVE op joins the retention pass too —
                            // a SendReply mid-PUT when the ACL died is
                            // exactly the reply worth retrying (it was
                            // silently dropped here before 2026-07-13).
                            if let Some(op) = active_map_op.take() {
                                if matches!(
                                    op,
                                    PendingMapOp::SendReply { .. } | PendingMapOp::FetchMessage { .. }
                                ) {
                                    pending_map_ops.push_front(op);
                                }
                            }
                            pending_map_ops.retain(|op| match op {
                                PendingMapOp::SendReply {
                                    queued_at,
                                    recipient_phone,
                                    ..
                                } => {
                                    if queued_at.elapsed() < SEND_REPLY_RETAIN_TTL {
                                        true
                                    } else {
                                        // Aged out: surface it — a customer
                                        // was told a text was coming.
                                        let _ = event_tx.send(RuntimeEvent::SmsSendFailed {
                                            recipient_phone: recipient_phone.clone(),
                                            reason: format!(
                                                "abandoned {:.0}s after queueing (ACL lost before the phone acked the send)",
                                                queued_at.elapsed().as_secs_f32()
                                            ),
                                        });
                                        false
                                    }
                                }
                                PendingMapOp::FetchMessage { queued_at, .. } => {
                                    queued_at.elapsed() < SEND_REPLY_RETAIN_TTL
                                }
                                _ => false,
                            });
                            let preserved = pending_map_ops.len();
                            if map_runtime.is_some() || pre_count > 0 {
                                eprintln!(
                                    "[AokieRadio] ACL handle {:#06x} dropped — \
                                     preserved {} fresh SendReply/FetchMessage op(s) \
                                     for retry; discarded {} stale/non-reply op(s) \
                                     plus {} active MAP runtime",
                                    connection_handle,
                                    preserved,
                                    pre_count - preserved,
                                    if map_runtime.is_some() { 1 } else { 0 }
                                );
                            }
                            map_runtime = None;
                            active_map_op = None;
                            map_idle_since = None;
                            mns_subscription_attempted_for_acl = false;
                            mns_subscribe_attempted_at = None;
                            pbap_pending_for_acl = false;
                            pbap_pending_since = None;
                            // Keep `seen_handles` and `inbox_poll_seeded`
                            // across the disconnect: handles are
                            // persistent on the AG side, and re-seeding
                            // on the next ACL would mark a freshly
                            // arrived (but not-yet-fetched) message as
                            // "already there at session start" and
                            // silently drop it instead of fetching its
                            // body. The set's memory cost is one short
                            // string per inbox handle — tiny.
                            last_inbox_poll = None;
                            // The "quiet PollInbox after a SendReply"
                            // gate only matters on the same MAS
                            // session — a fresh ACL gives us a fresh
                            // MAS, so drop the stamp.
                            last_send_reply_at = None;
                            // Reset MAS-stall watchdog state: a fresh
                            // ACL session deserves a fresh threshold,
                            // and any stage-2 escalation that fired to
                            // get us here is now satisfied.
                            mas_recovery_attempted_at = None;
                            last_acl_inbound_at = Instant::now();
                            // Drop any half-buffered ACL bytes from the
                            // dead link so a partial fragment can't
                            // misalign parsing on the next ACL session.
                            if !acl_buffer.is_empty() {
                                eprintln!(
                                    "[AokieRadio] ACL handle {:#06x} dropped — discarding {} buffered ACL bytes",
                                    connection_handle,
                                    acl_buffer.len()
                                );
                                acl_buffer.clear();
                            }
                            acl_partial_since = None;
                            // Userspace buffer is clean, but the WinUSB
                            // kernel ring may still hold a couple of
                            // bytes of stale L2CAP signalling for the
                            // dead handle. Without flushing the bulk-IN
                            // pipe those bytes prepend onto the first
                            // read of the next ACL session, manifesting
                            // as a 2-byte `88 e0 ...` misalignment that
                            // wedges SDP/RFCOMM bring-up for ~60s until
                            // the peer gives up. Same root cause
                            // documented at `flush_in_pipes()` for the
                            // app-restart path; this is the mid-session
                            // sibling.
                            if let Err(e) = transport.flush_acl_in_pipe() {
                                eprintln!("[AokieRadio] post-disconnect ACL flush failed: {}", e);
                            }
                            // Reset (don't rebuild) the MnsServer so
                            // the same Arc references in the L2CAP
                            // closures remain valid for the next ACL.
                            if let Ok(mut g) = mns_server.lock() {
                                g.reset();
                            }
                            // Drop the codec cache: a fresh ACL means a
                            // fresh peer state, so any sticky mSBC/CVSD
                            // selection from the previous session is no
                            // longer authoritative. The next call's
                            // CodecSelected will refill it.
                            selected_codec = None;
                        }
                        if active_sco_handle == Some(*connection_handle) {
                            active_sco_handle = None;
                            active_sco_tx_packet_len = None;
                            sco_tx_next_packet_at = None;
                            // Drop any unfinished TTS audio from this
                            // call so it can't leak into the next call's
                            // greeting once a fresh SCO link comes up.
                            sco_tx_queue.clear();
                            // And drop any half-assembled inbound SCO
                            // bytes so the framing of the next call's
                            // first packet isn't shifted by leftover
                            // bytes from this one.
                            sco_assembler.reset();
                            // Fresh H2 codec state for the next call so
                            // the rotating sync sequence starts at 0x08,
                            // the loss detector doesn't fire on the
                            // first inbound packet, and partial frame
                            // bytes don't leak across calls.
                            h2_decoder = H2Decoder::new();
                            msbc_rx_framer.reset();
                            msbc_tx_packager.reset();
                            msbc_rx_diag = MsbcRxDiag::new();
                            if let Err(e) = transport.disable_sco_alt_setting() {
                                eprintln!("[AokieRadio] disable_sco_alt failed: {}", e);
                            }
                            status.call_active.store(false, Ordering::Relaxed);
                            status.sample_rate.store(0, Ordering::Relaxed);
                            answer_sent_for_call = false;
                            // Keep `selected_codec` sticky across SCO
                            // disconnect. Pixel/Bluedroid caches the
                            // negotiated codec on the AG side and on
                            // back-to-back calls fires the next SCO
                            // ConnectionRequest *before* sending +BCS,
                            // expecting the prior codec's link
                            // parameters. If we reset to None here the
                            // accept lands in the CVSD don't-care branch
                            // while the peer wants mSBC/T2 and the
                            // controller times out (status 0x10) twice
                            // before the phone gives up its cache and
                            // sends +BCS=1. ~4-5 s of dead air follows.
                            // Cleared on ACL drop (truly new peer state)
                            // and overwritten on the next CodecSelected.
                            let _ = event_tx.send(RuntimeEvent::AudioDisconnected);
                            // A supervision timeout (0x08) or remote power-off
                            // (0x15) on the SCO handle means the whole RF link
                            // died — the ACL DisconnectionComplete lands right
                            // behind this one and the consumer's device-loss
                            // path owns the truthful teardown (call.ended
                            // reason "device_lost"). A CallTerminated here
                            // would mislabel the dropped link as a normal
                            // remote/operator hangup (observed live
                            // 2026-07-13: mid-call supervision timeout was
                            // recorded as reason remote_or_operator).
                            if !matches!(*reason, 0x08 | 0x15) {
                                let _ = event_tx.send(RuntimeEvent::CallTerminated);
                            }
                        }
                    }
                }
            }
            Err(e) if manager::is_timeout_error(&e) => {}
            Err(e) => return Err(e),
        }

        // 4c) ACL traffic (L2CAP signalling, SDP, RFCOMM/HFP control).
        //
        // WinUSB hands us a raw byte stream, not a packet stream. A
        // single read may return a tail-only chunk, or two ACL packets
        // concatenated, or a head fragment that needs the next read to
        // complete. We append the read into `acl_buffer` and drain
        // every complete ACL packet present, leaving any partial tail
        // for the next iteration.
        match transport.read_acl(max_acl_len) {
            Ok(packet) => {
                acl_buffer.extend_from_slice(&packet);
                // Bytes arriving = phone is talking to us at the link
                // layer, even if those bytes haven't yet completed a
                // frame our parser will drain. Stamping here (not just
                // on successful drain below) keeps the MAS-stall
                // watchdog from murdering an active ACL whenever our
                // accumulator is mid-frame on a slow inbox-listing
                // chunk. The frame-drain stamp at the bottom of this
                // loop still fires; this just adds a second source.
                last_acl_inbound_at = Instant::now();
            }
            Err(e) if manager::is_timeout_error(&e) => {}
            Err(e) => return Err(e),
        }

        loop {
            // Drain one ACL packet (or detect / recover from a
            // misaligned accumulator). The pure-mutation `pop_acl_frame`
            // owns the byte-level decisions — handler / sanity check /
            // partial-stall timer / resync — so the policy here is just
            // "log the diagnostic outcomes and dispatch the Frame".
            let pkt = match pop_acl_frame(
                &mut acl_buffer,
                &mut acl_partial_since,
                Instant::now(),
                active_acl_handle,
            ) {
                AclPopOutcome::NotReady | AclPopOutcome::Partial { .. } => break,
                AclPopOutcome::Resynced {
                    dropped_prefix,
                    declared,
                    buffer_len_before,
                    first32,
                } => {
                    eprintln!(
                        "[AokieRadio] ACL accumulator: corrupt header at offset 0 \
                         declares {}B; resyncing past {}-byte garbage prefix \
                         (buffer={}B, first 32=[{}])",
                        declared,
                        dropped_prefix,
                        buffer_len_before,
                        first32_hex(&first32),
                    );
                    note_acl_corruption(
                        &mut acl_corruption_times,
                        &mut acl_corruption_reported,
                        &event_tx,
                    );
                    continue;
                }
                AclPopOutcome::Flushed {
                    reason,
                    declared,
                    had,
                    first32,
                } => {
                    let dump = first32_hex(&first32);
                    match reason {
                        AclFlushReason::NoResyncTarget => {
                            eprintln!(
                                "[AokieRadio] ACL accumulator: corrupt header declares {} bytes \
                                 (>{}B sanity max), no resync target in {}B buffer — flushing \
                                 (first 32=[{}])",
                                declared, ACL_FRAME_SANITY_MAX, had, dump,
                            );
                        }
                        AclFlushReason::PartialStall(elapsed) => {
                            eprintln!(
                                "[AokieRadio] ACL accumulator stuck — partial frame \
                                 (declared {}B, have {}B) outstanding for {:?}; \
                                 flushing to resync (first 32=[{}])",
                                declared, had, elapsed, dump,
                            );
                        }
                    }
                    // Deliberately NOT counted toward the corruption report:
                    // a deterministic parser stall on one traffic shape (the
                    // MAP-poll loop produces an identical flush every cycle,
                    // live 2026-07-15) is a parsing bug to fix, not a wedged
                    // controller — telling the operator to replug for it was
                    // a false alarm. Only garbage-PREFIX resyncs (the real
                    // wedge signature) count.
                    break;
                }
                AclPopOutcome::Frame(pkt) => pkt,
            };
            // ACL parse / dispatch errors are non-fatal: a single
            // malformed packet should NOT kill the entire receptionist.
            // Log the byte dump so we can debug the root cause, drop
            // the offending packet, and keep the loop going so HFP /
            // MAP / PBAP recover on the next healthy packet.
            let responses = match l2cap_state.handle_acl_packet(&pkt) {
                Ok(r) => r,
                Err(e) => {
                    let dump = pkt
                        .iter()
                        .take(32)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");
                    eprintln!(
                        "[AokieRadio] ACL parse/dispatch error: {} (pkt len={}, first 32 bytes=[{}])",
                        e,
                        pkt.len(),
                        dump
                    );
                    Vec::new()
                }
            };
            // MAS-stall watchdog: any successfully-processed inbound
            // ACL counts as "the phone is alive at the link layer."
            // Stamp this even on dispatch errors? No — those would
            // mask Pixel-side breakage we want the watchdog to see.
            last_acl_inbound_at = Instant::now();
            for response in &responses {
                transport.write_acl(response)?;
            }
            // Phase 3e: drive PbapRuntime after every inbound ACL
            // packet so SDP / RFCOMM / OBEX progress lands on the
            // wire as soon as bytes arrive. Nothing to do when
            // there's no live fetch — the Option keeps this a
            // single-pointer load on the steady-state path.
            drive_pbap_runtime(&mut pbap_runtime, &mut l2cap_state, &transport, &event_tx);
            // HARD-001: drive the outbound HFP connect (phone.connect)
            // with the same cadence — SDP/RFCOMM progress lands on the
            // wire as soon as the phone's bytes arrive.
            drive_hfp_connect_runtime(
                &mut hfp_connect_runtime,
                active_acl_handle,
                &mut l2cap_state,
                &transport,
                &event_tx,
            );
            // Phase 4e: drain MnsServer events the inbound RFCOMM
            // closure may have produced as a side effect of the
            // ACL packet we just processed. NewMessage rows turn
            // into FetchMessage queue entries; everything else is
            // logged and dropped.
            //
            // Run BEFORE drive_map_runtime so a freshly-queued
            // FetchMessage suppresses MAP_IDLE_TIMEOUT — otherwise
            // an SMS arriving exactly when the pool is parked
            // tears MAS down before we get a chance to fetch the
            // body, and (on Pixel) the phone tears down our MNS
            // session in sympathy.
            drain_mns_events(&mns_server, &mut pending_map_ops, &mut seen_handles);
            // Phase 4e: same shape for the MAP runtime — ticks an
            // in-flight MAS operation forward, processes its
            // OperationCompleted output (emitting SmsReceived /
            // SmsSent / MapNotificationsSubscribed), and pulls
            // the next pending op off the queue.
            drive_map_runtime(
                active_acl_handle,
                &mut map_runtime,
                &mut active_map_op,
                &mut pending_map_ops,
                &mut map_idle_since,
                &mns_server,
                pbap_runtime.is_some(),
                &mut seen_handles,
                &mut inbox_poll_seeded,
                seed_attempts,
                &mut last_send_reply_at,
                &mut l2cap_state,
                &transport,
                &event_tx,
            );

            for hfp_event in l2cap_state.take_hfp_events() {
                manager::update_selected_codec(&mut selected_codec, &hfp_event);
                if let Err(e) = apply_codec_voice_setting(&transport, &hfp_event) {
                    let _ =
                        event_tx.send(RuntimeEvent::Error(format!("voice-setting switch: {}", e)));
                }
                let control_packets = manager::hfp_call_control_packets_for_event(
                    &mut l2cap_state,
                    &hfp_event,
                    &mut answer_sent_for_call,
                    &mut hfp_control,
                )?;
                for packet in &control_packets {
                    transport.write_acl(packet)?;
                }
                // HARD-001: same dead-half-link teardown as the heartbeat
                // drain — SLC failure on a link we paged drops the ACL.
                if outbound_connect_session {
                    if let HfpEvent::ServiceLevelConnectionFailed(_) = &hfp_event {
                        if let Some(handle) = active_acl_handle {
                            eprintln!(
                                "[AokieRadio] phone.connect: SLC failed on our paged link — disconnecting handle {:#06x}",
                                handle
                            );
                            let _ = transport.write_command(&hci::disconnect_command(handle, 0x13));
                        }
                        outbound_connect_session = false;
                    }
                }
                if matches!(hfp_event, HfpEvent::ServiceLevelConnectionReady) {
                    // Subscribe MAP first; defer PBAP until MNS is Connected (see pbap_pending_for_acl).
                    // Push_FRONT so preserved SendReply ops from the prior
                    // ACL session wait for MAS to come up before retrying.
                    if !mns_subscription_attempted_for_acl {
                        pending_map_ops.push_front(PendingMapOp::Subscribe);
                        mns_subscription_attempted_for_acl = true;
                        mns_subscribe_attempted_at = Some(Instant::now());
                        // PBAP is currently disabled by default on reconnect.
                        // Pixel's PSE flips SRM=enable on the first GET response,
                        // and our PCE doesn't honor SRM — so the phone stops
                        // streaming after the first 8KB chunk and we
                        // watchdog out at 15s. That 15-second gap is long
                        // enough that Pixel's MAS server times the OBEX
                        // session out on its end too, and from then on no
                        // SMS pushes come through and our PollInbox listing
                        // requests get no response.
                        //
                        // Caller-name lookup for incoming calls still works
                        // off the in-process contact_store cache (loaded
                        // from SQLite at startup), which was populated by
                        // earlier successful PBAP fetches before the
                        // streaming behaviour changed. The proper fix is to
                        // honor SRM on PCE GET continuations
                        // (project_pbap_srm_pse memory entry); until that
                        // lands, skipping PBAP keeps MAP/MNS healthy.
                        if PBAP_AUTO_FETCH_ON_RECONNECT {
                            pbap_pending_for_acl = true;
                            pbap_pending_since = Some(Instant::now());
                        } else {
                            eprintln!(
                                "[AokieRadio] PBAP auto-fetch disabled (avoids SRM stall \
                                 that wedges MAP). Contact lookups use the SQLite cache."
                            );
                        }
                    }
                }
                forward_hfp_event(hfp_event, &event_tx, &status, &interface.path);
            }
        }

        // Phase 4g.d: tick MAP every loop iteration regardless of
        // inbound ACL traffic. Without this, an op enqueued from the
        // control channel (typically SendSms produced by the auto-
        // reply pipeline reacting to an SmsReceived event) sits on
        // pending_map_ops indefinitely whenever the runtime is parked
        // in Resting — there's no inbound packet to wake the queue
        // drain inside the `Ok(packet)` arm above. The function is a
        // no-op when both the runtime and the queue are idle.
        // Tick PBAP every loop iteration too, not just per inbound
        // ACL. Without this the inactivity watchdog never fires when
        // Pixel's PSE strands us mid-fetch — `read_acl` returns
        // timeout each iteration, the inner drain loop exits without
        // calling drive_pbap_runtime, and PBAP sits in DrivingPbap{,Shared}
        // forever (blocking MAP because of `pbap_busy`).
        drive_pbap_runtime(&mut pbap_runtime, &mut l2cap_state, &transport, &event_tx);
        // HARD-001: tick the outbound HFP connect off the idle path too,
        // so its inactivity watchdog fires even when the phone goes
        // silent (no inbound ACL to wake the drain above).
        drive_hfp_connect_runtime(
            &mut hfp_connect_runtime,
            active_acl_handle,
            &mut l2cap_state,
            &transport,
            &event_tx,
        );
        drive_map_runtime(
            active_acl_handle,
            &mut map_runtime,
            &mut active_map_op,
            &mut pending_map_ops,
            &mut map_idle_since,
            &mns_server,
            pbap_runtime.is_some(),
            &mut seen_handles,
            &mut inbox_poll_seeded,
            seed_attempts,
            &mut last_send_reply_at,
            &mut l2cap_state,
            &transport,
            &event_tx,
        );

        // Inbox-poll scheduler. Runs only when MNS RFCOMM is healthy
        // (gating on the same MnsState the idle-disconnect check uses)
        // and no other MAS op is queued/active. Stamping last_inbox_poll
        // at enqueue time, not at completion, prevents the next loop
        // tick from re-queueing while the listing is still in flight.
        if active_acl_handle.is_some()
            && active_map_op.is_none()
            && pending_map_ops.is_empty()
            && pbap_runtime.is_none()
        {
            let mns_active = mns_server
                .lock()
                .map(|g| matches!(*g.state(), MnsState::Connected | MnsState::AssemblingPut))
                .unwrap_or(false);
            // MNS that never arrived: on an outbound (initiator-mux)
            // session the phone cannot open its notification DLCI
            // toward us, so MNS sits AwaitingConnect for the whole ACL.
            // Past the grace window the poll IS the inbound-SMS channel
            // — gating it on MNS Connected left a customer's reply
            // unread for the rest of the session (live 2026-07-13).
            // MAS itself is demonstrably healthy on these sessions
            // (Subscribe + SendReply complete in <1s), so the original
            // "don't poke a wedged phone" rationale doesn't apply.
            let mns_never_arrived = !mns_active
                && mns_subscribe_attempted_at
                    .is_some_and(|t| t.elapsed() >= MNS_ABSENT_POLL_GRACE);
            let poll_channel_ready = mns_active || mns_never_arrived;
            // Defer PollInbox if a SendReply finished recently. See
            // POLL_SKIP_AFTER_REPLY: poking dlci 11 with a SETPATH
            // while Pixel's MAS is still committing the just-PUT SMS
            // makes the AG go silent for the rest of the ACL.
            let send_reply_quiet = match last_send_reply_at {
                None => true,
                Some(ts) => ts.elapsed() >= POLL_SKIP_AFTER_REPLY,
            };
            if poll_channel_ready && send_reply_quiet {
                let due = match last_inbox_poll {
                    None => true,
                    Some(ts) => ts.elapsed() >= INBOX_POLL_INTERVAL,
                };
                if due {
                    pending_map_ops.push_back(PendingMapOp::PollInbox);
                    last_inbox_poll = Some(Instant::now());
                    if !inbox_poll_seeded {
                        seed_attempts = seed_attempts.saturating_add(1);
                    }
                }
            } else if poll_channel_ready && !send_reply_quiet {
                // Make the gate visible in logs once per cycle:
                // suppresses one log per 5s heartbeat-aligned window so
                // we can see "we wanted to poll but we were holding
                // off because SendReply was Xs ago" without spamming.
                if let Some(ts) = last_send_reply_at {
                    let elapsed = ts.elapsed();
                    let due = match last_inbox_poll {
                        None => true,
                        Some(p) => p.elapsed() >= INBOX_POLL_INTERVAL,
                    };
                    if due
                        && last_poll_skip_log_at
                            .map(|l: Instant| l.elapsed() >= Duration::from_secs(5))
                            .unwrap_or(true)
                    {
                        eprintln!(
                            "[AokieRadio] PollInbox deferred — SendReply was {:.1}s ago \
                             (skip window {:.0}s)",
                            elapsed.as_secs_f32(),
                            POLL_SKIP_AFTER_REPLY.as_secs_f32()
                        );
                        last_poll_skip_log_at = Some(Instant::now());
                    }
                }
            }
        }

        if pbap_pending_for_acl {
            let mns_ready = mns_server
                .lock()
                .map(|g| matches!(*g.state(), MnsState::Connected | MnsState::AssemblingPut))
                .unwrap_or(false);
            let fallback_elapsed = pbap_pending_since
                .map(|t| t.elapsed() >= Duration::from_secs(10))
                .unwrap_or(false);
            if mns_ready || fallback_elapsed {
                if !mns_ready {
                    eprintln!(
                        "[AokieRadio] PBAP fallback timer fired — MNS still not Connected after 10s, starting PBAP anyway"
                    );
                }
                start_pbap_fetch_if_idle(
                    active_acl_handle,
                    &mut pbap_runtime,
                    &mut l2cap_state,
                    &transport,
                    &event_tx,
                );
                pbap_pending_for_acl = false;
                pbap_pending_since = None;
            }
        }

        // 4d) SCO RX/TX (when the call has an audio channel up).
        if let Some(handle) = active_sco_handle {
            let is_msbc = selected_codec
                .as_ref()
                .map(|(codec, _)| codec.eq_ignore_ascii_case("mSBC"))
                .unwrap_or(false);

            match transport.read_sco(max_sco_len.min(255)) {
                Ok(bytes) => {
                    if !bytes.is_empty() {
                        last_sco_rx_bytes_at = Some(Instant::now());
                        sco_rx_silence_logged_at = None;
                        sco_dump::dump_rx(&bytes);
                    }
                    // The assembler self-clears its buffer on framing
                    // loss and returns Err; that's recoverable, not
                    // fatal. Don't `?` it through to the runtime —
                    // a single bad byte from a USB hiccup would
                    // otherwise kill the call.
                    match sco_assembler.push_bytes(&bytes) {
                        Ok(packets) => {
                            for packet in packets {
                                if is_msbc {
                                    process_msbc_rx_packet(
                                        &packet,
                                        &mut msbc_rx_framer,
                                        &mut h2_decoder,
                                        &audio_tx,
                                        &status,
                                        &mut msbc_rx_diag,
                                    );
                                } else {
                                    process_cvsd_rx_packet(&packet, &audio_tx, &status);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[AokieRadio] SCO assembler resync after framing loss: {}",
                                e
                            );
                        }
                    }
                }
                Err(e) if manager::is_timeout_error(&e) || e.contains("no HCI SCO in endpoint") => {
                }
                Err(e) => return Err(e),
            }

            // Outbound pacing. We must drain many packets per loop
            // iteration: each tick can take 50–200 ms (read timeouts),
            // but the SCO link consumes audio in tiny chunks (3 ms for
            // CVSD, 7.5 ms for mSBC). Cap at SCO_TX_MAX_PER_TICK so a
            // backed-up TTS burst can't starve the rest of the loop.
            //
            // Keep TX continuous for the lifetime of the SCO link in
            // both codecs. The original CVSD path stopped when the PCM
            // queue dried up between TTS bursts, on the theory that the
            // controller would produce CVSD silence on its own once the
            // FIFO drained. In practice, on CSR8510 the iso ring drains
            // to zero pending, the next write triggers a
            // ContinueStream=FALSE stream restart (Win32 error 87 →
            // re-arm path in `write_sco_isoch`), and that restart
            // empirically clicks audibly at every TTS-pause boundary.
            // Streaming zero-filled CVSD packets keeps `out_pending > 0`
            // and lets ContinueStream=TRUE chain across utterance gaps.
            // For mSBC: same rationale, plus the byte-level packager
            // backlog (mSBC frames are 60 bytes, but we write 48-byte
            // HCI payloads — one frame straddles 1.25 packets) which
            // strands mid-frame bytes if we stop mid-burst.
            let tx_active = active_sco_handle.is_some();
            if tx_active {
                let tx_plan = if is_msbc {
                    msbc_tx_plan(&init_report.buffer_size)
                } else {
                    cvsd_tx_plan(&init_report.buffer_size, active_sco_tx_packet_len)
                };

                if let Some(plan) = tx_plan {
                    let now = Instant::now();
                    // On cold start (very first TTS write of this SCO
                    // link, or first write after a 200 ms idle reset)
                    // pre-date the virtual schedule by ~30 ms so the
                    // first iteration writes ~10 packets and primes the
                    // controller's SCO TX FIFO. Without this prime the
                    // first iter only writes 1 packet, then the loop
                    // sits idle for the next 30 ms iter, which is long
                    // enough for the FIFO to underflow and produce a
                    // burst of static at the start of every TTS turn.
                    const SCO_TX_PRIME_MS: u64 = 30;
                    let next = sco_tx_next_packet_at
                        .unwrap_or_else(|| now - Duration::from_millis(SCO_TX_PRIME_MS));
                    // If we're very late (e.g. after a long silence)
                    // restart the clock at `now` so we don't try to
                    // catch up by dumping a wall of audio.
                    let next = if now > next + Duration::from_millis(200) {
                        now
                    } else {
                        next
                    };
                    let elapsed = now.saturating_duration_since(next);
                    let packets_due = 1 + (elapsed.as_micros() / plan.packet_interval_us) as usize;
                    const SCO_TX_MAX_PER_TICK: usize = 16;
                    let to_write = packets_due.min(SCO_TX_MAX_PER_TICK);
                    let mut written = 0;
                    let mut last_err: Option<String> = None;
                    // Capture the first packet of the burst so we can
                    // dump its head bytes in the per-burst log: a
                    // valid mSBC HCI packet is 3 bytes SCO header
                    // (handle + len + status) followed by mSBC bytes
                    // that, on a frame boundary, start with H2 sync
                    // 0x01 0x{08,38,c8,f8} (rotating). If the head
                    // never shows H2 sync, the packager isn't
                    // emitting valid mSBC.
                    let mut first_packet_head: Option<[u8; 8]> = None;
                    // Snapshot the queue depth before the burst so the
                    // log can suppress noise from "writing silence
                    // because TTS is paused" while still surfacing real
                    // audio bursts. With continuous mSBC TX we'd
                    // otherwise emit a log line every ~30 ms forever.
                    let queue_len_before = sco_tx_queue.len();
                    while written < to_write {
                        // No early-break on empty queue: CVSD's
                        // `pop_sco_packet` pads with zeros (silence) when
                        // the queue runs dry, which keeps the iso ring
                        // primed for ContinueStream=TRUE chaining across
                        // TTS pauses. mSBC stays continuous for the same
                        // reason plus its byte-level packager backlog.
                        let packet = if is_msbc {
                            build_msbc_sco_packet(
                                handle,
                                &mut sco_tx_queue,
                                &mut msbc_tx_packager,
                                plan.payload_len,
                            )?
                        } else {
                            sco_tx_queue.pop_sco_packet(handle, plan.payload_len)?
                        };
                        if first_packet_head.is_none() {
                            let mut head = [0u8; 8];
                            let n = packet.len().min(8);
                            head[..n].copy_from_slice(&packet[..n]);
                            first_packet_head = Some(head);
                        }
                        sco_dump::dump_tx(&packet);
                        match transport.write_sco(&packet) {
                            Ok(()) => written += 1,
                            Err(e) => {
                                last_err = Some(e);
                                break;
                            }
                        }
                    }
                    if written > 0 {
                        sco_tx_next_packet_at = Some(
                            next + Duration::from_micros(plan.packet_interval_us as u64)
                                * (written as u32),
                        );
                    }
                    let log_burst = last_err.is_some() || (written > 0 && queue_len_before > 0);
                    if log_burst {
                        eprintln!(
                            "[AokieRadio] SCO TX: wrote {} packet(s) of {} bytes (queue depth {} samples remain, codec={}, head={:02x?}){}",
                            written,
                            plan.payload_len,
                            sco_tx_queue.len(),
                            if is_msbc { "mSBC" } else { "CVSD" },
                            first_packet_head.unwrap_or([0; 8]),
                            match &last_err {
                                Some(e) => format!(" — write error: {}", e),
                                None => String::new(),
                            },
                        );
                    }
                    if let Some(e) = last_err {
                        let _ = event_tx.send(RuntimeEvent::Error(format!("sco write: {}", e)));
                    }
                }
            } else {
                // No audio to send — let the next greeting/turn arm
                // a fresh wall-clock when it starts.
                sco_tx_next_packet_at = None;
            }
        }
    }
}

fn forward_hci_event(
    event: &hci::HciEvent,
    active_sco_handle: Option<u16>,
    event_tx: &UnboundedSender<RuntimeEvent>,
    status: &Arc<RuntimeStatus>,
) {
    match event {
        hci::HciEvent::ConnectionComplete {
            status: cstatus,
            connection_handle,
            address,
            link_type,
            ..
        } if *cstatus == 0 && *link_type == hci::LINK_TYPE_ACL => {
            eprintln!(
                "[AokieRadio] ACL Connection Complete from {} (handle {:#06x})",
                address, connection_handle
            );
            status.connected.store(true, Ordering::Relaxed);
            if let Ok(mut a) = status.addresses.write() {
                a.remote = address.clone();
            }
            let _ = event_tx.send(RuntimeEvent::DeviceConnected(address.clone()));
        }
        hci::HciEvent::DisconnectionComplete {
            status: dstatus,
            connection_handle,
            reason,
            ..
        } if *dstatus == 0 => {
            // DisconnectionComplete doesn't carry a link_type field, so
            // we have to identify the SCO handle ourselves to avoid
            // tearing the ACL "DeviceConnected" status down when only
            // the SCO link drops at end-of-call. Without this filter,
            // the desktop UI flips back to "connecting…" the moment a
            // call ends even though the ACL session is still alive.
            // The SCO-specific cleanup (AudioDisconnected, queue
            // reset, alt-setting teardown) lives on the main loop and
            // runs whether or not we forward here.
            if Some(*connection_handle) == active_sco_handle {
                return;
            }
            // Track the previous remote address so the event payload
            // mirrors what the UI saw on connect, then clear it.
            let remote = status
                .addresses
                .read()
                .map(|a| a.remote.clone())
                .unwrap_or_default();
            eprintln!(
                "[AokieRadio] Disconnection Complete handle {:#06x} reason 0x{:02x} ({}) — was {}",
                connection_handle,
                reason,
                disconnect_reason_name(*reason),
                if remote.is_empty() {
                    "<unknown>"
                } else {
                    remote.as_str()
                }
            );
            if let Ok(mut a) = status.addresses.write() {
                a.remote.clear();
                a.remote_name = None;
            }
            status.connected.store(false, Ordering::Relaxed);
            let _ = event_tx.send(RuntimeEvent::DeviceDisconnected(remote));
        }
        _ => {}
    }
}

/// Common HCI Disconnect reason codes — surfaced in the desktop log so
/// "phone dropped after a minute" tells us *why* (supervision timeout vs
/// AG-side hangup vs auth failure, etc.).
fn disconnect_reason_name(reason: u8) -> &'static str {
    match reason {
        0x05 => "auth failure",
        0x08 => "supervision timeout",
        0x13 => "remote user terminated",
        0x14 => "remote: low resources",
        0x15 => "remote: power off",
        0x16 => "local terminated",
        0x1a => "unsupported remote feature",
        0x22 => "lmp response timeout",
        _ => "see HCI spec",
    }
}

/// Phase 3e: kick off a PBAP fetch when HFP service-level connection
/// completes, provided we know the ACL handle and don't already have
/// a fetch in flight. Errors during start are downgraded to logged
/// `RuntimeEvent::Error` — PBAP is best-effort, so a phone that
/// refuses our ConnectionRequest (security block, no PBAP service,
/// etc.) shouldn't poison the HFP path.
fn start_pbap_fetch_if_idle(
    active_acl_handle: Option<u16>,
    pbap_runtime: &mut Option<PbapRuntime>,
    l2cap_state: &mut l2cap::L2capState,
    transport: &AokieHciTransport,
    event_tx: &UnboundedSender<RuntimeEvent>,
) {
    if pbap_runtime.is_some() {
        return;
    }
    let Some(handle) = active_acl_handle else {
        eprintln!(
            "[AokieRadio] PBAP start skipped — no active ACL handle (HFP ready before ACL ConnectionComplete?)"
        );
        return;
    };
    let mut runtime = PbapRuntime::new(handle);
    match runtime.start(l2cap_state) {
        Ok(packets) => {
            eprintln!(
                "[AokieRadio] PBAP fetch starting on ACL handle {:#06x}",
                handle
            );
            for packet in &packets {
                if let Err(e) = transport.write_acl(packet) {
                    let _ =
                        event_tx.send(RuntimeEvent::Error(format!("PBAP start ACL write: {}", e)));
                    return;
                }
            }
            *pbap_runtime = Some(runtime);
        }
        Err(e) => {
            let _ = event_tx.send(RuntimeEvent::Error(format!("PBAP start: {}", e)));
        }
    }
}

/// HARD-001 phone.connect: the security handshake WE drive on a link we
/// paged, between ConnectionComplete and SDP. Sequence: send
/// Authentication_Requested (controller resolves the Link_Key_Request
/// from the pairing store) → Authentication_Complete → send
/// Set_Connection_Encryption → Encryption_Change → start the HFP
/// connect runtime. The phone does all of this itself on the inbound
/// path; skipping it outbound made the phone drop the link with 0x05
/// the moment our RFCOMM ConnectionRequest arrived.
struct ManualConnectAuth {
    connection_handle: u16,
    address: String,
    /// false = Authentication_Complete pending; true = it landed OK and
    /// Encryption_Change is pending.
    awaiting_encryption: bool,
    started_at: Instant,
}

/// HARD-001: drive the outbound HFP connect runtime by one tick.
/// Mirrors `drive_pbap_runtime`, with one difference on failure: a
/// paged ACL whose profile setup failed is a dead half-link the phone
/// will neither use nor repair, so we disconnect it (0x13) instead of
/// leaving a misleading "connected" LED. The SLC outcome itself flows
/// through `take_hfp_events` like any inbound connection.
fn drive_hfp_connect_runtime(
    hfp_connect_runtime: &mut Option<HfpConnectRuntime>,
    active_acl_handle: Option<u16>,
    l2cap_state: &mut l2cap::L2capState,
    transport: &AokieHciTransport,
    event_tx: &UnboundedSender<RuntimeEvent>,
) {
    let Some(runtime) = hfp_connect_runtime.as_mut() else {
        return;
    };
    match runtime.tick(l2cap_state) {
        Ok(packets) => {
            for packet in &packets {
                if let Err(e) = transport.write_acl(packet) {
                    let _ = event_tx.send(RuntimeEvent::Error(format!(
                        "HFP connect tick ACL write: {}",
                        e
                    )));
                    *hfp_connect_runtime = None;
                    return;
                }
            }
        }
        Err(e) => {
            eprintln!("[AokieRadio] HFP connect tick error: {}", e);
            *hfp_connect_runtime = None;
            return;
        }
    }
    for ev in runtime.take_events() {
        match ev {
            HfpConnectEvent::SlcKicked => {
                eprintln!("[AokieRadio] phone.connect: outbound SLC kicked");
            }
            HfpConnectEvent::Failed(reason) => {
                let _ = event_tx.send(RuntimeEvent::Error(format!(
                    "phone.connect failed: {}",
                    reason
                )));
                // Tear the half-open ACL down so the phone can cleanly
                // reconnect (either direction) rather than sitting on a
                // dead link.
                if let Some(handle) = active_acl_handle {
                    let cmd = hci::disconnect_command(handle, 0x13);
                    if let Err(e) = transport.write_command(&cmd) {
                        eprintln!(
                            "[AokieRadio] phone.connect: teardown disconnect write failed: {}",
                            e
                        );
                    }
                }
            }
        }
    }
    if runtime.is_done() || runtime.is_failed() {
        *hfp_connect_runtime = None;
    }
}

/// Phase 3e: drive the PBAP runtime forward by one tick. Drains the
/// inbound buffer (filled by L2CAP per-channel handler closures
/// during `handle_acl_packet`), produces outbound ACL packets, and
/// surfaces `PbapContactsFetched` events. The runtime is consumed
/// (set back to `None`) once the fetch reaches `Done` or `Failed`
/// so the next ACL connection's SLC can start a fresh attempt.
fn drive_pbap_runtime(
    pbap_runtime: &mut Option<PbapRuntime>,
    l2cap_state: &mut l2cap::L2capState,
    transport: &AokieHciTransport,
    event_tx: &UnboundedSender<RuntimeEvent>,
) {
    let Some(runtime) = pbap_runtime.as_mut() else {
        return;
    };
    match runtime.tick(l2cap_state) {
        Ok(packets) => {
            for packet in &packets {
                if let Err(e) = transport.write_acl(packet) {
                    let _ =
                        event_tx.send(RuntimeEvent::Error(format!("PBAP tick ACL write: {}", e)));
                    // A transport write failure usually means the ACL
                    // is gone; clear the runtime so we stop pumping.
                    *pbap_runtime = None;
                    return;
                }
            }
        }
        Err(e) => {
            // Protocol error during tick — log and stop driving.
            // PBAP is optional; we don't want it to take HFP down.
            eprintln!("[AokieRadio] PBAP tick error: {}", e);
            *pbap_runtime = None;
            return;
        }
    }
    for ev in runtime.take_events() {
        match ev {
            PbapRuntimeEvent::ContactsFetched(contacts) => {
                let total: Vec<RuntimeContact> = contacts
                    .iter()
                    .flat_map(|c| {
                        c.phone_numbers.iter().map(|number| RuntimeContact {
                            phone_number: number.clone(),
                            display_name: c.display_name.clone(),
                        })
                    })
                    .collect();
                eprintln!(
                    "[AokieRadio] PBAP fetch complete — {} contacts ({} (number, name) pairs)",
                    contacts.len(),
                    total.len()
                );
                let _ = event_tx.send(RuntimeEvent::PbapContactsFetched(total));
            }
            PbapRuntimeEvent::Failed(reason) => {
                // Don't surface as RuntimeEvent::Error — PBAP failure
                // is non-fatal (some phones simply don't expose PBAP
                // PSE, or block it pending bond confirmation). The
                // greeting path falls back to number-only.
                eprintln!("[AokieRadio] PBAP fetch failed: {}", reason);
            }
        }
    }
    if runtime.is_done() || runtime.is_failed() {
        *pbap_runtime = None;
    }
}

// ============================================================
// Phase 4e: MAP runtime + MNS server channel wiring.
// ============================================================

/// MAS instance id we always talk to. MAP servers usually advertise
/// instance 0 for SMS in the SDP record's MASInstanceID app-param;
/// non-zero is reserved for email / IM. Hardcoding 0 covers every
/// Pixel / iPhone we've tested. TODO: parse the AG's MAS SDP record
/// to discover the right instance per phone.
const MAS_INSTANCE_ID_FOR_SMS: u8 = 0;

/// Install the MAP MNS server-channel into newly-created RfcommState
/// instances. We override the L2CAP PSM_RFCOMM handler so any RFCOMM
/// channel the AG opens gets a fresh `RfcommState` with our MNS
/// tenant pre-registered. Any extras a future profile needs (PBAP
/// PSE server, etc.) plug in here too.
///
/// `wbs_supported` is plumbed onto every freshly-created RfcommState so
/// the HFP SLC AT queue advertises mSBC in `AT+BAC` only when the
/// transport actually exposes a BTstack-parity 8-bit SCO alt-setting.
/// Caller derives this from `transport.supports_msbc_alt_setting()`
/// before booting the L2CAP loop.
fn install_mns_server_channel(
    l2cap_state: &mut l2cap::L2capState,
    mns_server: Arc<StdMutex<MnsServer>>,
    wbs_supported: bool,
    call_waiting_enabled: bool,
) {
    l2cap_state.register_psm(
        l2cap::PSM_RFCOMM,
        Arc::new(move |channel, payload| {
            let mns_arc = mns_server.clone();
            let rfcomm_state = channel.rfcomm_state.get_or_insert_with(|| {
                let mut state = RfcommState::new();
                state.set_wbs_supported(wbs_supported);
                state.set_call_waiting_enabled(call_waiting_enabled);
                register_mns_server_channel_handlers(&mut state, mns_arc.clone());
                state
            });
            rfcomm_state.handle_packet(payload)
        }),
    );
}

/// Register the MNS RFCOMM server-channel handlers on a fresh
/// `RfcommState`. The handlers wrap each MnsServer reply in a UIH
/// frame on the right DLCI; the runtime tick loop drains pending
/// events from the same shared MnsServer.
fn register_mns_server_channel_handlers(
    state: &mut RfcommState,
    mns_server: Arc<StdMutex<MnsServer>>,
) {
    let dlci = server_channel_dlci(AOKIE_MNS_RFCOMM_CHANNEL, false);
    let mns_for_uih = mns_server.clone();
    let mns_for_disc = mns_server.clone();
    let on_sabm: Arc<dyn Fn() -> Vec<Vec<u8>> + Send + Sync> = Arc::new(move || {
        // Mirror the HFP SABM reply pattern: UA on this DLCI plus
        // a proactive MSC CMD on the multiplexer so the AG knows
        // we're ready to receive UIH right away. Without the MSC
        // most AGs hold UIH until they get one from us.
        vec![
            build_ua(dlci, true),
            build_uih(
                RFCOMM_DLCI_MULTIPLEXER,
                false,
                None,
                &build_modem_status_command(dlci, RFCOMM_LOCAL_MODEM_STATUS),
            ),
        ]
    });
    let on_disc: Arc<dyn Fn() -> Vec<u8> + Send + Sync> = Arc::new(move || {
        if let Ok(mut g) = mns_for_disc.lock() {
            g.reset();
        }
        build_ua(dlci, true)
    });
    let on_uih: Arc<dyn Fn(&[u8]) -> Result<Vec<Vec<u8>>, String> + Send + Sync> =
        Arc::new(move |payload| {
            let mut guard = mns_for_uih
                .lock()
                .map_err(|_| "MNS mutex poisoned".to_string())?;
            // feed_bytes returns one OBEX response per request. Each
            // needs its own UIH frame on this DLCI. CR=true because
            // we're emitting from the responder side toward the
            // initiator (the AG); RFCOMM uses the convention
            // initiator=true on commands and =false on responses.
            let obex_responses = guard.feed_bytes(payload);
            let mut frames = Vec::with_capacity(obex_responses.len());
            for resp in obex_responses {
                frames.push(build_uih(dlci, false, None, &resp));
            }
            Ok(frames)
        });
    state.register_server_channel(
        AOKIE_MNS_RFCOMM_CHANNEL,
        ServerChannelHandlers {
            on_sabm,
            on_disc,
            on_uih,
        },
    );
}

/// Phase 4e: tick the MapRuntime, process its OperationCompleted
/// output, and start the next pending op if the slot is free.
/// Mirrors `drive_pbap_runtime`'s shape — keep the two side-by-side
/// in the IO loop so anyone reading runtime.rs sees the parallel.
///
/// `pbap_busy` defers starting a fresh MAP op while PBAP is mid-fetch
/// on the same shared RFCOMM mux. Racing both OBEX flows on the same
/// peer makes Pixel queue MAS responses behind the PBAP body stream;
/// PBAP can take 10s+ on a large phonebook and MAS CONNECT silently
/// stalls behind it. An already-running MAP op (`map_runtime` already
/// Some) is allowed to keep ticking — only fresh op starts wait.
fn drive_map_runtime(
    active_acl_handle: Option<u16>,
    map_runtime: &mut Option<MapRuntime>,
    active_map_op: &mut Option<PendingMapOp>,
    pending_map_ops: &mut VecDeque<PendingMapOp>,
    map_idle_since: &mut Option<std::time::Instant>,
    mns_server: &Arc<StdMutex<MnsServer>>,
    pbap_busy: bool,
    seen_handles: &mut HashSet<String>,
    inbox_poll_seeded: &mut bool,
    seed_attempts: usize,
    last_send_reply_at: &mut Option<Instant>,
    l2cap_state: &mut l2cap::L2capState,
    transport: &AokieHciTransport,
    event_tx: &UnboundedSender<RuntimeEvent>,
) {
    // First, tick the live runtime if any.
    if let Some(runtime) = map_runtime.as_mut() {
        match runtime.tick(l2cap_state) {
            Ok(packets) => {
                for packet in &packets {
                    if let Err(e) = transport.write_acl(packet) {
                        let _ = event_tx
                            .send(RuntimeEvent::Error(format!("MAP tick ACL write: {}", e)));
                        *map_runtime = None;
                        *active_map_op = None;
                        *map_idle_since = None;
                        return;
                    }
                }
            }
            Err(e) => {
                // Non-fatal — log and drop. SMS auto-reply degrades
                // gracefully; HFP keeps working.
                eprintln!("[AokieRadio] MAP tick error: {}", e);
                *map_runtime = None;
                *active_map_op = None;
                *map_idle_since = None;
            }
        }
    }
    if let Some(runtime) = map_runtime.as_mut() {
        for ev in runtime.take_events() {
            handle_map_runtime_event(
                ev,
                active_map_op,
                pending_map_ops,
                seen_handles,
                inbox_poll_seeded,
                seed_attempts,
                last_send_reply_at,
                event_tx,
            );
        }
        if runtime.is_done() || runtime.is_failed() {
            *map_runtime = None;
            *active_map_op = None;
            *map_idle_since = None;
        } else if runtime.is_resting() {
            // Phase 4g.d: pooled runtime finished its op and parked.
            // The handler above already emitted the right
            // RuntimeEvent for `active_map_op`; clear it now so the
            // bookkeeping reflects "no op in flight" while we wait
            // for either a queued op or the idle timeout.
            *active_map_op = None;
        }
    }
    // Phase 4g.d: pooled runtime parked in Resting after an op
    // completed. Decision tree:
    //   - queue has another op → feed it via start_next_op (zero
    //     reconnect overhead, just SETPATH + op-specific request).
    //   - queue empty → stamp map_idle_since (if not already) and
    //     wait for either a new op or the idle timeout.
    if let Some(runtime) = map_runtime.as_mut() {
        if runtime.is_resting() {
            // Same gate as the slot-free path: don't issue fresh OBEX on the resting MAS while
            // PBAP is mid-stream — Pixel queues MAS responses behind PBAP body chunks and stalls.
            if !pbap_busy && !pending_map_ops.is_empty() {
                if let Some(next_op) = pending_map_ops.pop_front() {
                    feed_pooled_next_op(
                        runtime,
                        next_op,
                        active_map_op,
                        map_idle_since,
                        pending_map_ops,
                        l2cap_state,
                        transport,
                        event_tx,
                    );
                }
            } else if pending_map_ops.is_empty() && map_idle_since.is_none() {
                *map_idle_since = Some(std::time::Instant::now());
            }
        }
    }
    // After a feed_pooled_next_op failure the runtime may have been
    // dropped (so a fresh one can be built for the re-queued op);
    // also handle that "slot free with pending ops" case by falling
    // through to the regular start path below.
    // Phase 4g.d: idle-timeout check. If the pooled runtime has been
    // resting alone for > MAP_IDLE_TIMEOUT, send DISCONNECT so we
    // don't hold the RFCOMM channel forever (some phones cap the
    // number of concurrent MAP sessions and won't let a fresh
    // notification through with us still hogging the slot).
    //
    // BUT: if the phone has an active MNS session pushing
    // notifications back to us, dropping MAS would tear MNS down too
    // (Pixel/Bluedroid pairs them — closing MAS triggers Pixel's
    // BluetoothMapClient.disconnect path which closes its outbound
    // MNS RFCOMM in sympathy). That kills the SMS-receive path until
    // the next ACL re-pair. So skip the idle disconnect while MNS is
    // mid-session — we'd rather hold a quiet MAS socket than lose
    // notifications. AwaitingConnect / Disconnected / Failed all mean
    // MNS isn't currently load-bearing, so recycling MAS is safe.
    let mns_active = mns_server
        .lock()
        .map(|g| matches!(*g.state(), MnsState::Connected | MnsState::AssemblingPut))
        .unwrap_or(false);
    if let (Some(runtime), Some(since)) = (map_runtime.as_mut(), *map_idle_since) {
        if runtime.is_resting() && since.elapsed() >= MAP_IDLE_TIMEOUT && !mns_active {
            eprintln!(
                "[AokieRadio] MAP pooled session idle for {:?} — disconnecting",
                since.elapsed()
            );
            match runtime.request_disconnect(l2cap_state) {
                Ok(packets) => {
                    for packet in &packets {
                        if let Err(e) = transport.write_acl(packet) {
                            let _ = event_tx.send(RuntimeEvent::Error(format!(
                                "MAP idle-disconnect ACL write: {}",
                                e
                            )));
                        }
                    }
                }
                Err(e) => eprintln!("[AokieRadio] MAP idle-disconnect: {}", e),
            }
            *map_idle_since = None;
        }
    }
    // Slot free? Pull the next pending op — but defer until PBAP
    // is done (see fn-doc).
    if map_runtime.is_none() && !pending_map_ops.is_empty() && !pbap_busy {
        if let Some(op) = pending_map_ops.pop_front() {
            start_map_operation(
                op,
                active_acl_handle,
                map_runtime,
                active_map_op,
                l2cap_state,
                transport,
                event_tx,
            );
        }
    }
}

/// Phase 4g.d: feed a queued op into a resting pooled runtime. Wraps
/// the start_next_op call with the standard transport-write +
/// active_map_op-bookkeeping the orchestrator does for fresh ops.
///
/// Takes `pending_map_ops` so it can re-queue the op if start_next_op
/// refuses (e.g. a state-machine bug puts the session in an
/// unexpected phase). The orchestrator's "slot free → start fresh"
/// branch then re-establishes the connection from scratch and pulls
/// the re-queued op off the front.
fn feed_pooled_next_op(
    runtime: &mut MapRuntime,
    next_op: PendingMapOp,
    active_map_op: &mut Option<PendingMapOp>,
    map_idle_since: &mut Option<std::time::Instant>,
    pending_map_ops: &mut VecDeque<PendingMapOp>,
    l2cap_state: &mut l2cap::L2capState,
    transport: &AokieHciTransport,
    event_tx: &UnboundedSender<RuntimeEvent>,
) {
    let mas_op = pending_op_to_mas_operation(&next_op);
    match runtime.start_next_op(mas_op, l2cap_state) {
        Ok(packets) => {
            eprintln!(
                "[AokieRadio] MAP pooled op {} feeding into resting session",
                next_op.log_summary()
            );
            for packet in &packets {
                if let Err(e) = transport.write_acl(packet) {
                    let _ = event_tx.send(RuntimeEvent::Error(format!(
                        "MAP pooled-op ACL write: {}",
                        e
                    )));
                    return;
                }
            }
            *active_map_op = Some(next_op);
            *map_idle_since = None;
        }
        Err(e) => {
            eprintln!(
                "[AokieRadio] MAP start_next_op failed: {} — re-queuing {} \
                 and dropping pooled session for re-establishment",
                e,
                next_op.log_summary()
            );
            // Push the op back to the FRONT so it's the very next
            // thing tried. The orchestrator's slot-free branch will
            // construct a fresh runtime and pull it.
            pending_map_ops.push_front(next_op);
            // The session refused — likely a state-machine bug. Best
            // recovery is a full re-establish; tell the runtime to
            // disconnect (best-effort) so the AG-side state matches
            // what we'll do on reconnect.
            let _ = runtime.request_disconnect(l2cap_state);
            *map_idle_since = None;
        }
    }
}

/// Map a queued PendingMapOp to the MAS-level operation. Shared by
/// the fresh-runtime start path and the pooled feed-next-op path so
/// they can't drift.
fn pending_op_to_mas_operation(op: &PendingMapOp) -> MasOperation {
    match op {
        PendingMapOp::Subscribe => MasOperation::SetNotificationRegistration { enabled: true },
        PendingMapOp::FetchMessage { handle, .. } => MasOperation::GetMessage {
            folder: MasFolder::Inbox,
            handle: handle.clone(),
            charset: CHARSET_UTF8,
        },
        PendingMapOp::SendReply { bmessage, .. } => MasOperation::PushMessage {
            folder: MasFolder::Outbox,
            bmessage: bmessage.clone(),
            charset: CHARSET_UTF8,
        },
        PendingMapOp::PollInbox => MasOperation::ListMessages {
            folder: MasFolder::Inbox,
            max_list_count: INBOX_POLL_MAX_LIST,
            // Both SMS variants — the AG only stores whichever its radio
            // produces, and including both costs us nothing on phones
            // that don't have one of them.
            filter_message_type: MSG_TYPE_KEEP_BOTH_SMS_VARIANTS,
        },
    }
}

/// How long a queued `SendReply` survives an ACL teardown or a failed
/// MAS attempt. A reply generated seconds before a stall recovery is
/// still worth sending once a healthy session is back; one queued
/// minutes ago is stale, and a surprise late delivery would be worse
/// than the honest `SmsSendFailed` it surfaces as. (Module-level so
/// both the radio loop and `handle_map_runtime_event` share one TTL.)
const SEND_REPLY_RETAIN_TTL: Duration = Duration::from_secs(120);

/// How long after queueing the MAP Subscribe we keep waiting for the
/// phone's MNS connection before the inbox poll takes over as THE
/// inbound-SMS channel. On phone-initiated sessions MNS connects within
/// ~2s of the Subscribe ack, so 20s cleanly separates "still coming"
/// from "not coming on this session shape" (outbound/initiator-mux
/// sessions, where the phone cannot open its notification DLCI to us).
const MNS_ABSENT_POLL_GRACE: Duration = Duration::from_secs(20);

/// How long one MAS op may stay in flight before the watchdog treats
/// it as wedged even though the ACL itself is healthy (keepalive
/// replies keep the link-silence clock fresh, so ACL silence alone
/// never fires for a mid-OBEX wedge). Healthy ops — including the
/// 5-entry inbox listing — complete in well under a second.
const MAP_OP_DEADLINE: Duration = Duration::from_secs(30);

/// How often to poll the inbox listing as a backstop for dropped MNS
/// pushes. Pixel's MAP intermittency is unpredictable — sometimes
/// every notification arrives, sometimes one in three. 45 s strikes a
/// balance between catch-up latency and wasted MAS traffic on quiet
/// threads. The poll only runs when no other MAS op is in flight.
const INBOX_POLL_INTERVAL: Duration = Duration::from_secs(45);

/// Top-N handles per poll. Listing returns newest-first.
///
/// **Why so small (was 50, now 5)**: with 50, the XML body lands at
/// ~14 KB, which is well past the threshold where Pixel's MAS PSE
/// flips on SRM and starts streaming chunks back-to-back. Even with
/// SRM honoured correctly on our side, Pixel intermittently freezes
/// mid-stream and stops sending the terminating RSP_OK forever. Every
/// time PollInbox triggers that on a fresh ACL, the MAS-stall
/// watchdog fires, the link gets torn down, and we cycle endlessly
/// — observed across multiple test sessions on 2026-04-28.
///
/// At 5 entries the body is ~1.5 KB, well under the negotiated OBEX
/// MaxPacketLength (16 KB), so the entire response fits in a single
/// OBEX packet with EndOfBody — no SRM, no continuations, no stall.
/// The trade-off: if MNS ever drops more than 5 messages in a single
/// 45 s window we'd miss the older ones, but real SMS inbound rates
/// are nowhere near that. Bump cautiously if needed.
const INBOX_POLL_MAX_LIST: u16 = 5;

/// How long after a SendReply completion (AG ack of our PUT) the
/// inbox poller stays quiet. Empirically, issuing a SETPATH on dlci
/// 11 within tens of seconds of a successful PUT to /outbox makes
/// Pixel's MAS go silent for the rest of the ACL — the catatonic
/// shared-mux symptom. The likely cause is the AG being busy
/// committing the SMS to its cellular queue / SQLite store and
/// dropping the next OBEX request rather than queueing it.
///
/// 20 s is the trade-off point: long enough that Pixel's
/// post-PUT settling window is over (most SMS sends complete in
/// a few seconds, even on weak signal), short enough that a
/// genuine MNS push drop during the window only delays a salvage
/// poll by a fifth of the previous 60 s budget. If the gate proves
/// too aggressive (recovery cycle firing every conversation), bump
/// back up — but at 60 s a customer's follow-up that actually came
/// through MAP was deaf-listened past the point they'd give up.
const POLL_SKIP_AFTER_REPLY: Duration = Duration::from_secs(20);

/// Phase 4g.d: how long a pooled MAP session sits in Resting before
/// we force DISCONNECT. Five seconds covers the gap between a
/// FetchMessage completing and Gemma producing the auto-reply (which
/// takes 500ms-2s on the test device) without holding the connection
/// open across long quiet periods.
const MAP_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Translate a `MapRuntimeEvent` into the runtime-event surface the
/// Tauri layer consumes. Knowing which op the runtime was fulfilling
/// is what makes a generic `OperationCompleted` resolve to a
/// specific event variant.
fn handle_map_runtime_event(
    event: MapRuntimeEvent,
    active_map_op: &Option<PendingMapOp>,
    pending_map_ops: &mut VecDeque<PendingMapOp>,
    seen_handles: &mut HashSet<String>,
    inbox_poll_seeded: &mut bool,
    seed_attempts: usize,
    last_send_reply_at: &mut Option<Instant>,
    event_tx: &UnboundedSender<RuntimeEvent>,
) {
    match event {
        MapRuntimeEvent::OperationCompleted(output) => match (active_map_op.as_ref(), output) {
            (Some(PendingMapOp::Subscribe), _) => {
                eprintln!("[AokieRadio] MAP NotificationRegistration acked");
                let _ = event_tx.send(RuntimeEvent::MapNotificationsSubscribed);
            }
            (
                Some(PendingMapOp::FetchMessage { handle, .. }),
                crate::aokie_radio::map_mas::OperationOutput::Message(bytes),
            ) => {
                let parsed = bmessage::parse(&bytes);
                let _ = event_tx.send(RuntimeEvent::SmsReceived {
                    sender_phone: parsed.sender_addressing.unwrap_or_default(),
                    sender_name: parsed.sender_name,
                    body: parsed.body,
                    handle: handle.clone(),
                    msg_type: parsed.msg_type,
                });
            }
            (
                Some(PendingMapOp::SendReply {
                    recipient_phone, ..
                }),
                _,
            ) => {
                *last_send_reply_at = Some(Instant::now());
                let _ = event_tx.send(RuntimeEvent::SmsSent {
                    recipient_phone: recipient_phone.clone(),
                });
            }
            (
                Some(PendingMapOp::PollInbox),
                crate::aokie_radio::map_mas::OperationOutput::Listing(bytes),
            ) => {
                handle_inbox_listing(
                    &bytes,
                    pending_map_ops,
                    seen_handles,
                    inbox_poll_seeded,
                    seed_attempts,
                );
            }
            (Some(other), unexpected) => {
                eprintln!(
                    "[AokieRadio] MAP OperationCompleted with mismatched output {:?} for op {}",
                    unexpected,
                    other.log_summary()
                );
            }
            (None, _) => {
                eprintln!("[AokieRadio] MAP OperationCompleted with no active op");
            }
        },
        MapRuntimeEvent::Failed(reason) => {
            eprintln!("[AokieRadio] MAP runtime failed: {}", reason);
            // Subscribe/PollInbox failures don't surface — they're
            // best-effort (the SLC handler and the poll timer re-drive
            // them). A failed SendReply is DIFFERENT: it's a customer's
            // text. Fresh ones re-queue for the next MAS session
            // (bounded by SEND_REPLY_RETAIN_TTL, so a deterministic
            // refusal can't loop forever); aged ones surface as
            // SmsSendFailed so nothing dies silently.
            if let Some(op @ PendingMapOp::SendReply { .. }) = active_map_op.as_ref() {
                let (queued_at, recipient_phone) = match op {
                    PendingMapOp::SendReply {
                        queued_at,
                        recipient_phone,
                        ..
                    } => (*queued_at, recipient_phone.clone()),
                    _ => unreachable!(),
                };
                if queued_at.elapsed() < SEND_REPLY_RETAIN_TTL {
                    eprintln!(
                        "[AokieRadio] re-queueing failed SendReply (queued {:.1}s ago)",
                        queued_at.elapsed().as_secs_f32()
                    );
                    pending_map_ops.push_back(op.clone());
                } else {
                    let _ = event_tx.send(RuntimeEvent::SmsSendFailed {
                        recipient_phone,
                        reason: format!("MAS session failed the send: {}", reason),
                    });
                }
            }
        }
    }
}

/// Diff a fresh inbox listing against `seen_handles` and queue
/// FetchMessage for anything new. The very first listing of an ACL
/// session seeds `seen_handles` instead — without this, the bot would
/// flood-fetch every existing inbox row on startup and auto-reply to
/// each one. Once seeded, only new arrivals trigger a fetch.
///
/// `seed_attempts` is the count of times PollInbox has been queued
/// without a successful seed yet. On the FIRST attempt (count == 1)
/// we seed conservatively — the inbox state at app start is treated
/// as historical, none of it gets auto-replied. On a RETRY attempt
/// (count >= 2, meaning a previous queue stalled before completing),
/// we additionally fetch the top entry: the customer's message that
/// arrived during the dead session is most likely sitting at the top
/// of the listing, and silently dropping it is exactly the symptom
/// the recovery cycle is meant to fix.
/// True when a listing entry's local timestamp falls within the last
/// hour of OUR local wall clock. MAP datetimes are ISO-8601 basic with
/// the PHONE's local time ("20260713T185500+1000"); the phone and this
/// desktop share a room, so comparing local wall-clock prefixes is
/// reliable in practice. Entries with no/short datetime pass (a phone
/// that omits the attribute must not block the catch-up).
fn entry_is_recent(entry: &crate::aokie_radio::map_listing::MessageEntry) -> bool {
    let Some(dt) = entry.datetime.as_deref() else {
        return true;
    };
    // get(..15) — never panics on a non-char-boundary (the listing body
    // is from_utf8_lossy'd, so garbage bytes become multibyte chars).
    let Some(prefix) = dt.get(..15) else {
        return true;
    };
    let cutoff = (chrono::Local::now() - chrono::Duration::hours(1))
        .format("%Y%m%dT%H%M%S")
        .to_string();
    prefix >= cutoff.as_str()
}

fn handle_inbox_listing(
    bytes: &[u8],
    pending_map_ops: &mut VecDeque<PendingMapOp>,
    seen_handles: &mut HashSet<String>,
    inbox_poll_seeded: &mut bool,
    seed_attempts: usize,
) {
    let entries = parse_listing(bytes);
    if !*inbox_poll_seeded {
        for entry in &entries {
            seen_handles.insert(entry.handle.clone());
        }
        *inbox_poll_seeded = true;
        let catchup_handle = if seed_attempts >= 2 {
            entries.first().map(|e| e.handle.clone())
        } else {
            // First seed: an UNREAD top-of-inbox message that arrived
            // RECENTLY is very likely one the customer sent while our
            // inbound path was down (plugin restart, or an MNS-less
            // outbound session before the poll's first pass — live
            // 2026-07-13: a follow-up reply sat stranded exactly here).
            // One bounded catch-up fetch; read history is never touched,
            // and anything older than the recency window stays seeded
            // ("a surprise late reply is worse than silence").
            entries
                .first()
                .filter(|e| e.read == Some(false) && entry_is_recent(e))
                .map(|e| e.handle.clone())
        };
        if let Some(handle) = catchup_handle {
            eprintln!(
                "[AokieRadio] inbox poll seeded with {} existing handle(s) (attempt #{}) \
                 — fetching top entry {} as catch-up (retry seed, or unread+recent arrival \
                 from the window before the first poll)",
                entries.len(),
                seed_attempts,
                handle
            );
            pending_map_ops.push_back(PendingMapOp::FetchMessage {
                handle,
                queued_at: Instant::now(),
            });
        } else {
            eprintln!(
                "[AokieRadio] inbox poll seeded with {} existing handle(s) — only new arrivals will be fetched",
                entries.len()
            );
        }
        return;
    }
    let mut queued = 0;
    for entry in &entries {
        if seen_handles.insert(entry.handle.clone()) {
            eprintln!(
                "[AokieRadio] inbox poll caught new handle {} (likely a missed MNS push) — queuing fetch",
                entry.handle
            );
            pending_map_ops.push_back(PendingMapOp::FetchMessage {
                handle: entry.handle.clone(),
                queued_at: Instant::now(),
            });
            queued += 1;
        }
    }
    if queued == 0 {
        eprintln!(
            "[AokieRadio] inbox poll: {} handle(s) listed, all already seen",
            entries.len()
        );
    }
}

/// Construct a `MapRuntime` for a pending op and kick off its first
/// SDP query. On success, stores the runtime in `map_runtime` and
/// records the op in `active_map_op`.
fn start_map_operation(
    op: PendingMapOp,
    active_acl_handle: Option<u16>,
    map_runtime: &mut Option<MapRuntime>,
    active_map_op: &mut Option<PendingMapOp>,
    l2cap_state: &mut l2cap::L2capState,
    transport: &AokieHciTransport,
    event_tx: &UnboundedSender<RuntimeEvent>,
) {
    let Some(handle) = active_acl_handle else {
        eprintln!("[AokieRadio] MAP op skipped — no active ACL handle");
        return;
    };
    let mas_op = pending_op_to_mas_operation(&op);
    let mut runtime = MapRuntime::new(handle, mas_op, MAS_INSTANCE_ID_FOR_SMS);
    // Phase 4g.d: opt into pooled mode so subsequent ops re-use this
    // OBEX/RFCOMM connection instead of paying SDP+CONNECT again.
    runtime.enable_pooling();
    match runtime.start(l2cap_state) {
        Ok(packets) => {
            eprintln!(
                "[AokieRadio] MAP op {} starting on ACL handle {:#06x}",
                op.log_summary(),
                handle
            );
            for packet in &packets {
                if let Err(e) = transport.write_acl(packet) {
                    let _ =
                        event_tx.send(RuntimeEvent::Error(format!("MAP start ACL write: {}", e)));
                    return;
                }
            }
            *map_runtime = Some(runtime);
            *active_map_op = Some(op);
        }
        Err(e) => {
            let _ = event_tx.send(RuntimeEvent::Error(format!("MAP start: {}", e)));
        }
    }
}

/// Drain `MnsEvent`s the inbound RFCOMM closure may have surfaced.
/// `NewMessage` rows enqueue a follow-up `FetchMessage` op so the
/// runtime fetches the body via MAS as soon as the slot frees.
/// Other events are logged and dropped.
fn drain_mns_events(
    mns_server: &Arc<StdMutex<MnsServer>>,
    pending_map_ops: &mut VecDeque<PendingMapOp>,
    seen_handles: &mut HashSet<String>,
) {
    let events = match mns_server.lock() {
        Ok(mut g) => g.take_events(),
        Err(_) => return,
    };
    for ev in events {
        match ev {
            MnsEvent::NewMessage {
                handle, msg_type, ..
            } => {
                if !seen_handles.insert(handle.clone()) {
                    eprintln!(
                        "[AokieRadio] MNS NewMessage handle={} type={:?} — already fetched via poll, skipping duplicate",
                        handle, msg_type
                    );
                    continue;
                }
                eprintln!(
                    "[AokieRadio] MNS NewMessage handle={} type={:?} — queuing fetch",
                    handle, msg_type
                );
                pending_map_ops.push_back(PendingMapOp::FetchMessage {
                    handle,
                    queued_at: Instant::now(),
                });
            }
            MnsEvent::Other { event_type, handle } => {
                eprintln!(
                    "[AokieRadio] MNS Other event {:?} handle={:?} — ignoring",
                    event_type, handle
                );
            }
            MnsEvent::Unparseable { reason } => {
                eprintln!("[AokieRadio] MNS unparseable EventReport: {}", reason);
            }
        }
    }
}

fn forward_hfp_event(
    event: HfpEvent,
    event_tx: &UnboundedSender<RuntimeEvent>,
    status: &Arc<RuntimeStatus>,
    interface_path: &str,
) {
    match event {
        HfpEvent::ServiceLevelConnectionReady => {
            eprintln!("[AokieRadio] HFP service-level connection ready");
        }
        HfpEvent::ServiceLevelConnectionFailed(reason) => {
            eprintln!("[AokieRadio] HFP SLC failed at {}", reason);
            let _ = event_tx.send(RuntimeEvent::Error(format!("HFP SLC failed at {}", reason)));
        }
        HfpEvent::IncomingCall => {
            eprintln!("[AokieRadio] HFP IncomingCall");
            let _ = event_tx.send(RuntimeEvent::CallIncoming);
        }
        HfpEvent::Ringing => {
            eprintln!("[AokieRadio] HFP Ringing");
            let _ = event_tx.send(RuntimeEvent::CallRinging);
        }
        HfpEvent::OutgoingDialing => {
            eprintln!("[AokieRadio] HFP OutgoingDialing (MO call setup)");
            let _ = event_tx.send(RuntimeEvent::OutgoingDialing);
        }
        HfpEvent::CallAnswered => {
            eprintln!("[AokieRadio] HFP CallAnswered");
            let _ = event_tx.send(RuntimeEvent::CallAnswered);
        }
        HfpEvent::CallTerminated => {
            eprintln!("[AokieRadio] HFP CallTerminated");
            status.call_active.store(false, Ordering::Relaxed);
            // Per-call telemetry summary. Goes through the structured
            // audit channel so a future support flow can grep for
            // `call_terminated` lines and aggregate them into a
            // dongle-compatibility matrix without parsing free-form
            // logs. We log at every call boundary, not just when a
            // counter spikes — the cumulative numbers are what tell
            // us "this dongle resets 800 times every call" vs.
            // "5 resets in normal operation".
            aokie_core::redact::audit(
                "call_terminated",
                format!(
                    "interface={} sample_rate={} dropped_audio_frames={} sco_tx_stream_resets={}",
                    interface_path,
                    status.sample_rate.load(Ordering::Relaxed),
                    status.audio_dropped.load(Ordering::Relaxed),
                    crate::aokie_radio::sco_tx_stream_resets(),
                ),
            );
            let _ = event_tx.send(RuntimeEvent::CallTerminated);
        }
        HfpEvent::CallerId(number) => {
            eprintln!(
                "[AokieRadio] HFP CallerId {}",
                aokie_core::redact::Phone(&number)
            );
            let _ = event_tx.send(RuntimeEvent::CallerId(number));
        }
        HfpEvent::CallWaiting(number) => {
            eprintln!(
                "[AokieRadio] HFP CallWaiting — second caller knocking ({})",
                number
                    .as_deref()
                    .map(|n| aokie_core::redact::Phone(n).to_string())
                    .unwrap_or_else(|| "number not yet known".to_string()),
            );
            let _ = event_tx.send(RuntimeEvent::CallWaiting { number });
        }
        HfpEvent::CallWaitingEnded => {
            eprintln!("[AokieRadio] HFP CallWaitingEnded — waiting caller gone, active call untouched");
            let _ = event_tx.send(RuntimeEvent::CallWaitingEnded);
        }
        HfpEvent::CallHeld(state) => {
            eprintln!("[AokieRadio] HFP CallHeld indicator -> {state} (0 none / 1 held+active / 2 held only)");
            let _ = event_tx.send(RuntimeEvent::CallHeld { state });
        }
        HfpEvent::CallListEntry(entry) => {
            // Observe-only topology (Phase 4 step 2): every CLCC line is
            // logged AND forwarded — the plugin keeps the last snapshot in
            // dongle.diagnostics so a knock's topology is verifiable after
            // the fact (the 500-line log ring wraps in ~1 min under call
            // load; the first two live soak tests both lost the race), and
            // the switchboard slice reconciles switches against exactly
            // these entries.
            eprintln!(
                "[AokieRadio] HFP CLCC entry: idx={} dir={} status={} mode={} mpty={} number={}",
                entry.index,
                entry.direction,
                entry.status,
                entry.mode,
                entry.multiparty,
                entry
                    .number
                    .as_deref()
                    .map(|n| aokie_core::redact::Phone(n).to_string())
                    .unwrap_or_else(|| "-".to_string()),
            );
            let _ = event_tx.send(RuntimeEvent::CallListEntry {
                index: entry.index,
                direction: entry.direction,
                status: entry.status,
                multiparty: entry.multiparty,
                number: entry.number,
            });
        }
        HfpEvent::CodecSelected { codec, sample_rate } => {
            // Surface AG-initiated codec connection so a "no audio after
            // ATA" log shows whether the AG even ran the CC procedure
            // (no CodecSelected line = AG never sent +BCS = AC won't
            // start either, since per HFP 1.7 §4.11.2 AC requires CC
            // to have completed at least once).
            eprintln!(
                "[AokieRadio] HFP CodecSelected codec={} rate={}Hz",
                codec, sample_rate
            );
            // Stash the negotiated sample rate so it shows up in
            // `is_*` queries even before the SCO link is up. The
            // actual `AudioConnected` event fires from the main loop
            // when SynchronousConnectionComplete arrives.
            status.sample_rate.store(sample_rate, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ScoTxPlan {
    payload_len: usize,
    packet_interval_us: u128,
}

/// Pick payload length and pacing interval for CVSD outbound packets.
/// Returns `None` if the link parameters resolved to a zero-length
/// payload (no audio can be sent until the link's `tx_packet_length`
/// reports something usable).
fn cvsd_tx_plan(
    buffer_size: &hci::BufferSize,
    active_sco_tx_packet_len: Option<usize>,
) -> Option<ScoTxPlan> {
    // Use the BTstack-equivalent USB-aligned payload size (48 bytes =
    // 24 samples = 3 ms audio) so each HCI packet maps to exactly 3
    // isoch frames at alt 2 (3 × 17 = 51 byte HCI packet). Without
    // this, 60-byte payloads (4 frames = 4 ms USB time per 3.75 ms of
    // audio) chronically underrun the controller's SCO FIFO and
    // produce silence on the air.
    //
    // Also clamp to the SCO link's negotiated tx_packet_length so each
    // HCI packet maps to exactly one air packet (no controller-side
    // fragmentation for short links like HV3 = 30 bytes). Filter out
    // 0 — the spec allows tx_packet_length=0 for transparent-data air
    // mode, and an unfiltered 0 would `min` the payload down to nothing
    // and silently stop sending audio.
    let link_max = active_sco_tx_packet_len
        .filter(|&n| n > 0)
        .unwrap_or(AOKIE_SCO_USB_PAYLOAD_BYTES);
    let payload_len = manager::max_sco_payload_len(buffer_size)
        .min(AOKIE_SCO_USB_PAYLOAD_BYTES)
        .min(link_max)
        & !1;
    if payload_len == 0 {
        return None;
    }
    // 8 kHz × 16-bit linear PCM = 16 audio bytes per ms, so the
    // wall-clock interval per packet is `payload_len / 16` ms
    // = `payload_len * 62.5 µs`. Integer µs (× 1000 / 16) avoids floats.
    let packet_interval_us = (payload_len as u128) * 1000 / 16;
    Some(ScoTxPlan {
        payload_len,
        packet_interval_us,
    })
}

/// Linear-PCM (CVSD) inbound: extract i16 LE samples from the SCO
/// payload and forward them to the audio channel at the link's sample
/// rate. Bad payloads are dropped silently so a single transient framing
/// glitch can't kill the call.
fn process_cvsd_rx_packet(
    packet: &[u8],
    audio_tx: &Sender<AudioFrame>,
    status: &Arc<RuntimeStatus>,
) {
    let Ok(parsed) = sco::parse_sco_packet(packet) else {
        return;
    };
    let Ok(mut samples) = sco::linear_pcm_samples_from_payload(parsed.payload) else {
        return;
    };
    if samples.is_empty() {
        return;
    }
    // Per HFP/HCI spec, a non-zero packet_status_flag means the
    // controller flagged this CVSD payload as unreliable: the most
    // common values are 0b01 (no data received) and 0b11 (data
    // partially lost). CVSD has no per-frame CRC the way mSBC does,
    // so the status flag is our only loss signal — feeding the raw
    // bytes through anyway plays back whatever the controller had
    // sitting in its FIFO, which on a real call is audible static.
    // Silence-fill keeps the playback timeline aligned without
    // pretending we have audio we don't.
    if parsed.packet_status_flag != 0 {
        for s in samples.iter_mut() {
            *s = 0;
        }
    }
    let sample_rate = status.sample_rate.load(Ordering::Relaxed);
    if audio_tx
        .try_send(AudioFrame {
            samples,
            sample_rate,
        })
        .is_err()
    {
        // Channel full — the consumer (Whisper/VAD/bot loop) is
        // running too far behind real time. Drop this frame rather
        // than blocking the SCO RX path, and bump the diag counter so
        // it surfaces in `dropped_audio_frames()`.
        status.audio_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Throttled diagnostic state for the mSBC RX path.
///
/// We emit one summary line per second instead of per-frame logs (133
/// frames/sec at 7.5 ms each would otherwise flood the console). The
/// summary captures:
///  - decode success/failure counts since the last summary
///  - the last raw HCI SCO payload's status flag and first 16 bytes
///    (so we can see what comes off the wire before any of our
///    framing / decoding runs)
///  - the most recent decode error string and the first 16 bytes of
///    the frame it rejected (so we can compare against the expected
///    `0xAD 0x01 0x1A …` mSBC header)
struct MsbcRxDiag {
    last_log: Instant,
    decode_ok: u64,
    decode_err: u64,
    /// H2 sequence-gap events (one or more packets missing between
    /// adjacent successful decodes). Distinct from `decode_err`, which
    /// counts CRC / sync failures on bytes we DID receive.
    lost_packets: u64,
    /// HCI SCO payloads with a non-zero packet status flag. Their bytes
    /// are still fed to the framer (CRC catches the worst), but a
    /// non-zero rate here points at controller-side buffer underruns or
    /// air losses that no codec-level fix can recover.
    bad_status_packets: u64,
    last_payload_head: Option<(u8, usize, [u8; 16])>,
    last_err: Option<(String, [u8; 16])>,
    /// Total decoded samples seen in this window (across all successful
    /// decodes). Drives the RMS calculation below.
    sample_count: u64,
    /// Largest absolute sample value seen this window. `i32` to dodge
    /// the `i16::MIN.abs()` overflow.
    sample_peak: i32,
    /// Sum of squares of |sample| across this window. `u128` so a
    /// second's worth of speech (≈16k samples × ≤ 32_768²) can't
    /// overflow.
    sample_sum_sq: u128,
}

impl MsbcRxDiag {
    fn new() -> Self {
        Self {
            // Start in the past so the first call emits immediately —
            // useful when the stream produces zero successful decodes
            // and we'd otherwise wait a full second before any output.
            last_log: Instant::now() - Duration::from_secs(2),
            decode_ok: 0,
            decode_err: 0,
            lost_packets: 0,
            bad_status_packets: 0,
            last_payload_head: None,
            last_err: None,
            sample_count: 0,
            sample_peak: 0,
            sample_sum_sq: 0,
        }
    }

    fn record_decoded(&mut self, samples: &[i16]) {
        self.sample_count += samples.len() as u64;
        for &s in samples {
            let mag = (s as i32).abs();
            if mag > self.sample_peak {
                self.sample_peak = mag;
            }
            self.sample_sum_sq += (mag as u128) * (mag as u128);
        }
    }

    fn flush_if_due(&mut self) {
        if self.last_log.elapsed() < Duration::from_secs(1) {
            return;
        }
        if self.decode_ok == 0 && self.decode_err == 0 && self.last_payload_head.is_none() {
            return;
        }
        if let Some((status_flag, total, head)) = self.last_payload_head {
            eprintln!(
                "[AokieRadio] mSBC RX diag: ok={} err={} lost={} bad_status={} last_payload(status={:#04x}, len={}, head={:02x?})",
                self.decode_ok, self.decode_err, self.lost_packets, self.bad_status_packets, status_flag, total, head,
            );
        } else {
            eprintln!(
                "[AokieRadio] mSBC RX diag: ok={} err={} lost={} bad_status={} (no payloads yet)",
                self.decode_ok, self.decode_err, self.lost_packets, self.bad_status_packets,
            );
        }
        if self.sample_count > 0 {
            let mean_sq = (self.sample_sum_sq / self.sample_count as u128) as f64;
            let rms = mean_sq.sqrt();
            eprintln!(
                "[AokieRadio] mSBC RX diag: audio level samples={} peak={} rms={:.0} (i16 range ±32768)",
                self.sample_count, self.sample_peak, rms,
            );
        }
        if let Some((err, frame_head)) = &self.last_err {
            eprintln!(
                "[AokieRadio] mSBC RX diag: last decode err: {} frame_head={:02x?}",
                err, frame_head,
            );
        }
        self.last_log = Instant::now();
        self.decode_ok = 0;
        self.decode_err = 0;
        self.lost_packets = 0;
        self.bad_status_packets = 0;
        self.last_err = None;
        self.sample_count = 0;
        self.sample_peak = 0;
        self.sample_sum_sq = 0;
    }
}

/// mSBC inbound: append the SCO payload to the byte-stream framer and
/// drain any 60-byte H2 frames it produces. The controller fragments
/// 60-byte mSBC air frames into HCI-buffer-sized chunks (typically 48
/// bytes on USB dongles), so the framer is the part that re-aligns the
/// stream and resyncs after loss; the H2Decoder then validates each
/// recovered frame's sync, CRC, and sequence.
fn process_msbc_rx_packet(
    packet: &[u8],
    framer: &mut MsbcStreamFramer,
    decoder: &mut H2Decoder,
    audio_tx: &Sender<AudioFrame>,
    status: &Arc<RuntimeStatus>,
    diag: &mut MsbcRxDiag,
) {
    let parsed = match sco::parse_sco_packet(packet) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("[AokieRadio] mSBC RX: SCO parse failed: {}", e);
            return;
        }
    };
    // Capture the payload's status flag + head bytes for the next diag
    // flush. A non-zero status flag means the controller flagged this
    // payload as unreliable (no data / partially lost / possibly invalid),
    // which would explain a stream that never decodes.
    let mut head = [0u8; 16];
    let n = parsed.payload.len().min(16);
    head[..n].copy_from_slice(&parsed.payload[..n]);
    diag.last_payload_head = Some((parsed.packet_status_flag, parsed.payload.len(), head));
    if parsed.packet_status_flag != 0 {
        diag.bad_status_packets += 1;
    }

    framer.push(parsed.payload);
    while let Some(frame) = framer.next_frame() {
        let sample_rate = status.sample_rate.load(Ordering::Relaxed);
        // PLC: when the H2 sequence number jumped, fill the gap with
        // concealment frames built by H2Decoder so downstream playback
        // keeps a coherent wall-clock timeline. The decoder fades from
        // the last good frame (1.0 → 0.5 → 0.25 → silence), which masks
        // single-slot drops entirely and softens longer drops to a
        // muffle instead of a click. A naive zero-fill produced an
        // audible pop on every dropped iso slot during heavy TTS bursts
        // (Win32 error 87 cancels SCO IN URBs while the OUT pipe is
        // saturated).
        //
        // The gap count comes from H2Decoder rather than a fixed `1`
        // because the H2 sequence is mod-4: a 2-packet drop with `lost`
        // alone would shrink the timeline by 7.5 ms (one missing frame
        // worth of audio), which is audible on hardware as a click.
        let send_frame = |samples: Vec<i16>| {
            if audio_tx
                .try_send(AudioFrame {
                    samples,
                    sample_rate,
                })
                .is_err()
            {
                status.audio_dropped.fetch_add(1, Ordering::Relaxed);
            }
        };
        match decoder.decode_packet(&frame) {
            Ok(decoded) => {
                diag.decode_ok += 1;
                diag.record_decoded(&decoded.samples);
                if decoded.gap > 0 {
                    diag.lost_packets += decoded.gap as u64;
                    for conceal in &decoded.conceal {
                        send_frame(conceal.to_vec());
                    }
                }
                send_frame(decoded.samples.to_vec());
            }
            Err(e) => {
                diag.decode_err += 1;
                let mut frame_head = [0u8; 16];
                frame_head.copy_from_slice(&frame[..16]);
                diag.last_err = Some((format!("{:?}", e), frame_head));
                // Same rationale: conceal any predecessor packets we
                // missed, plus one for the rejected frame itself. The
                // decoder didn't update `last_pcm` on Err, so the fade
                // still anchors on the prior good frame.
                let gap = decoder.last_gap();
                if gap > 0 {
                    diag.lost_packets += gap as u64;
                }
                for conceal in decoder.conceal_lost(gap + 1) {
                    send_frame(conceal.to_vec());
                }
            }
        }
    }
    diag.flush_if_due();
}

/// Switch the controller's GLOBAL voice setting to match the codec the
/// AG just selected.
///
/// We set 0x0060 (CVSD, non-transparent) at controller init, which is
/// fine for narrow-band CVSD calls — the controller does the air coding.
/// For mSBC the controller must run "transparent" (0x0043 — BTstack-
/// parity, 8-bit input) so it passes our pre-encoded mSBC bytes through
/// verbatim instead of treating them as PCM and re-encoding. The Accept_Synchronous_Connection_Request
/// command carries a per-link voice_setting too, but several real-world
/// controllers (BTstack notes the same) ignore the per-link override and
/// fall back to whatever the most recent Write_Voice_Setting set, so the
/// host has to write the correct global value before the SCO link
/// comes up. Symptom of getting this wrong is exactly what we hit:
/// SCO ConnectionComplete succeeds, our mSBC TX bytes look valid in
/// the log, but the caller hears nothing and we get zero RX bytes for
/// the entire call.
fn apply_codec_voice_setting(
    transport: &AokieHciTransport,
    event: &HfpEvent,
) -> Result<(), String> {
    let target = match event {
        HfpEvent::CodecSelected { codec, .. } => {
            if codec.eq_ignore_ascii_case("mSBC") {
                AOKIE_VOICE_SETTING_TRANSPARENT
            } else {
                AOKIE_VOICE_SETTING
            }
        }
        // Don't reset to CVSD on CallTerminated. Pixel/Bluedroid caches
        // the negotiated codec on the AG side and may fire the next
        // call's SCO ConnectionRequest *before* sending +BCS, expecting
        // the prior codec's voice setting (e.g. 0x0043 transparent for
        // mSBC). If we wrote 0x0060 here the controller would re-encode
        // mSBC bytes as PCM and the negotiated link parameters would
        // mismatch what the peer asked for, giving Connection_Accept_
        // Timeout (0x10). Voice setting now stays at the last call's
        // value until the next CodecSelected overrides it.
        _ => return Ok(()),
    };
    let cmd = hci::write_voice_setting_command(target);
    transport.write_command(&cmd)?;
    eprintln!(
        "[AokieRadio] HCI Write_Voice_Setting -> {:#06x} (for codec event {:?})",
        target, event,
    );
    Ok(())
}

/// Pick the HCI SCO payload length and pacing interval for the mSBC
/// outbound stream.
///
/// 24 bytes (`AOKIE_SCO_USB_PAYLOAD_BYTES`) per BTStack reference
/// (`btstack/src/hci.c:4877-4885`) which hard-overrides the negotiated
/// SCO data packet length to 24 whenever the transport is USB. Mirrors
/// what BTStack-bridge produces on the same Broadcom 21ec dongle that
/// our 48-byte choice silenced. mSBC frames are 60 bytes, so one frame
/// straddles 2.5 HCI packets; the `MsbcStreamPackager` already handles
/// arbitrary chunk sizes by maintaining a byte-stream backlog and the
/// H2 sync recovery on the peer's framer doesn't care where USB-side
/// packet boundaries fall.
fn msbc_tx_plan(buffer_size: &hci::BufferSize) -> Option<ScoTxPlan> {
    let payload_len =
        manager::max_sco_payload_len(buffer_size).min(AOKIE_SCO_USB_PAYLOAD_BYTES) & !1;
    if payload_len == 0 {
        return None;
    }
    // 16 kHz mSBC carries 8 audio bytes per ms (60 bytes per 7.5 ms),
    // so an N-byte HCI SCO packet covers N / 8 ms of air time.
    let packet_interval_us = (payload_len as u128) * 1000 / MSBC_AUDIO_BYTES_PER_MS;
    Some(ScoTxPlan {
        payload_len,
        packet_interval_us,
    })
}

/// mSBC outbound: drain `payload_len` bytes from the encoded mSBC byte
/// stream and wrap them in an HCI SCO header. The packager encodes
/// additional 60-byte frames from `sco_tx_queue` on demand (padding
/// with silence if the queue runs short — silence is preferable to
/// underrunning the controller's SCO FIFO).
fn build_msbc_sco_packet(
    connection_handle: u16,
    sco_tx_queue: &mut sco::LinearPcmTxQueue,
    packager: &mut MsbcStreamPackager,
    payload_len: usize,
) -> Result<Vec<u8>, String> {
    let payload = packager.pop_bytes(payload_len, || {
        let mut frame = [0i16; MSBC_SAMPLES_PER_FRAME];
        sco_tx_queue.fill_frame(&mut frame);
        frame
    });
    sco::build_sco_packet(connection_handle, 0, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acl_corruption_reports_once_at_threshold() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut times = std::collections::VecDeque::new();
        let mut reported = false;
        for _ in 0..2 {
            note_acl_corruption(&mut times, &mut reported, &tx);
        }
        assert!(rx.try_recv().is_err(), "below threshold must stay a log line");
        note_acl_corruption(&mut times, &mut reported, &tx);
        match rx.try_recv() {
            Ok(RuntimeEvent::Error(msg)) => assert!(msg.contains("power-cycled")),
            other => panic!("expected the actionable error, got {other:?}"),
        }
        // One report per episode — continued corruption must not spam.
        note_acl_corruption(&mut times, &mut reported, &tx);
        assert!(rx.try_recv().is_err(), "no duplicate reports mid-episode");
    }

    fn listing_with(handles: &[&str]) -> Vec<u8> {
        let mut s = String::from("<MAP-msg-listing version=\"1.0\">");
        for h in handles {
            s.push_str(&format!("<msg handle=\"{}\" type=\"SMS_GSM\" />", h));
        }
        s.push_str("</MAP-msg-listing>");
        s.into_bytes()
    }

    #[test]
    fn first_listing_seeds_seen_handles_without_queueing_fetches() {
        let mut pending = VecDeque::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seeded = false;
        handle_inbox_listing(
            &listing_with(&["AAAA", "BBBB", "CCCC"]),
            &mut pending,
            &mut seen,
            &mut seeded,
            1, // first attempt — no catch-up fetch
        );
        assert!(seeded);
        assert!(pending.is_empty());
        assert_eq!(seen.len(), 3);
        assert!(seen.contains("AAAA") && seen.contains("BBBB") && seen.contains("CCCC"));
    }

    #[test]
    fn second_listing_queues_fetch_for_only_new_handles() {
        let mut pending = VecDeque::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seeded = false;
        handle_inbox_listing(
            &listing_with(&["AAAA", "BBBB"]),
            &mut pending,
            &mut seen,
            &mut seeded,
            1,
        );
        assert!(pending.is_empty()); // seeded
        handle_inbox_listing(
            &listing_with(&["CCCC", "AAAA", "BBBB"]),
            &mut pending,
            &mut seen,
            &mut seeded,
            1,
        );
        assert_eq!(pending.len(), 1);
        match pending.front() {
            Some(PendingMapOp::FetchMessage { handle, .. }) => assert_eq!(handle, "CCCC"),
            other => panic!("unexpected pending op: {:?}", other),
        }
    }

    #[test]
    fn handle_already_in_seen_via_mns_is_not_refetched_on_poll() {
        let mut pending = VecDeque::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seeded = true; // simulate post-seed state
        seen.insert("DEAD".to_string()); // MNS path inserted it earlier
        handle_inbox_listing(
            &listing_with(&["DEAD", "BEEF"]),
            &mut pending,
            &mut seen,
            &mut seeded,
            1,
        );
        assert_eq!(pending.len(), 1);
        match pending.front() {
            Some(PendingMapOp::FetchMessage { handle, .. }) => assert_eq!(handle, "BEEF"),
            other => panic!("unexpected pending op: {:?}", other),
        }
    }

    #[test]
    fn retry_seed_fetches_top_entry_for_dead_session_catchup() {
        // seed_attempts >= 2 means a previous PollInbox queue stalled
        // before completing. The next listing's top entry is most
        // likely the customer's message that arrived during the dead
        // window — fetch it instead of silently seeding past it.
        let mut pending = VecDeque::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seeded = false;
        handle_inbox_listing(
            &listing_with(&["FRESH", "OLD1", "OLD2"]),
            &mut pending,
            &mut seen,
            &mut seeded,
            2, // retry attempt
        );
        assert!(seeded);
        assert_eq!(seen.len(), 3, "all entries seeded into seen_handles");
        assert_eq!(pending.len(), 1, "exactly one catch-up fetch queued");
        match pending.front() {
            Some(PendingMapOp::FetchMessage { handle, .. }) => assert_eq!(handle, "FRESH"),
            other => panic!("unexpected pending op: {:?}", other),
        }
    }

    #[test]
    fn retry_seed_with_empty_listing_queues_no_catchup() {
        let mut pending = VecDeque::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut seeded = false;
        handle_inbox_listing(&listing_with(&[]), &mut pending, &mut seen, &mut seeded, 2);
        assert!(seeded);
        assert!(pending.is_empty(), "nothing to fetch when inbox is empty");
    }

    // ── ACL accumulator (pop_acl_frame) regression fixtures ─────────────
    //
    // WinUSB delivers ACL bytes as a stream, so the runtime's drain loop
    // must handle: complete frames, two frames concatenated in one read,
    // a head fragment whose tail arrives in the next read, garbage
    // prefixes from a misaligned restart, and partial frames that stay
    // outstanding past the wedge timeout. These cases are wrapped up in
    // `pop_acl_frame` so the byte-level decisions are unit-testable.
    fn acl_frame(handle: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + payload.len());
        // Point-to-point BC (=0), PB flags 0 — the only fields we mint
        // for these tests; the real wire uses PB=2 (start of HCI frag)
        // but pop_acl_frame doesn't inspect them.
        let header = handle & 0x0fff;
        out.extend_from_slice(&header.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn pop_acl_frame_drains_one_complete_frame() {
        let mut buf = acl_frame(0x0042, &[0xaa, 0xbb, 0xcc]);
        let mut partial = None;
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), None) {
            AclPopOutcome::Frame(pkt) => {
                assert_eq!(pkt, vec![0x42, 0x00, 0x03, 0x00, 0xaa, 0xbb, 0xcc]);
            }
            other => panic!("expected Frame, got {:?}", other),
        }
        assert!(buf.is_empty());
        assert!(partial.is_none());
    }

    #[test]
    fn pop_acl_frame_drains_two_concatenated_frames() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&acl_frame(0x0042, &[0x01]));
        buf.extend_from_slice(&acl_frame(0x0042, &[0x02]));
        let mut partial = None;
        let now = Instant::now();
        match pop_acl_frame(&mut buf, &mut partial, now, None) {
            AclPopOutcome::Frame(pkt) => assert_eq!(pkt[4], 0x01),
            other => panic!("first drain: {:?}", other),
        }
        match pop_acl_frame(&mut buf, &mut partial, now, None) {
            AclPopOutcome::Frame(pkt) => assert_eq!(pkt[4], 0x02),
            other => panic!("second drain: {:?}", other),
        }
        match pop_acl_frame(&mut buf, &mut partial, now, None) {
            AclPopOutcome::NotReady => {}
            other => panic!("expected NotReady once buffer drained: {:?}", other),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn pop_acl_frame_assembles_fragmented_frame_across_pushes() {
        let frame = acl_frame(0x0042, &[0xaa, 0xbb, 0xcc, 0xdd]);
        assert_eq!(frame.len(), 8);
        let mut buf = frame[..5].to_vec();
        let mut partial = None;
        let t0 = Instant::now();
        match pop_acl_frame(&mut buf, &mut partial, t0, None) {
            AclPopOutcome::Partial { declared, had } => {
                assert_eq!(declared, 8);
                assert_eq!(had, 5);
            }
            other => panic!("expected Partial: {:?}", other),
        }
        // Stamping happened on the first hit.
        assert_eq!(partial, Some(t0));
        // Tail arrives — the rest of the frame.
        buf.extend_from_slice(&frame[5..]);
        match pop_acl_frame(&mut buf, &mut partial, t0 + Duration::from_millis(20), None) {
            AclPopOutcome::Frame(pkt) => assert_eq!(pkt, frame),
            other => panic!("expected Frame after tail arrives: {:?}", other),
        }
        assert!(partial.is_none(), "successful drain clears partial_since");
    }

    #[test]
    fn pop_acl_frame_returns_not_ready_for_short_buffer_and_clears_stale_partial() {
        // <4 bytes can't even produce a header — must return NotReady
        // and clear any leftover partial timer (the previous frame
        // completed; the new tail is just bytes-of-the-next-header).
        let mut buf = vec![0x42, 0x00];
        let mut partial = Some(Instant::now() - Duration::from_secs(10));
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), None) {
            AclPopOutcome::NotReady => {}
            other => panic!("expected NotReady: {:?}", other),
        }
        assert!(
            partial.is_none(),
            "<4-byte buffer must clear partial_since to avoid false-positive stalls",
        );
        assert_eq!(buf.len(), 2, "buffer is preserved for the next push");
    }

    #[test]
    fn pop_acl_frame_resyncs_past_garbage_prefix() {
        // Prefix [0xff,0xff,0xff,0xff] declares 65 535 B → trips sanity
        // (>8 KB). pop_acl_frame should then scan forward, find the
        // valid header at offset 4, and report Resynced { dropped_prefix=4 }.
        let mut buf = vec![0xff, 0xff, 0xff, 0xff];
        let real = acl_frame(0x0042, &[0x01, 0x02, 0x03]);
        buf.extend_from_slice(&real);
        let mut partial = Some(Instant::now() - Duration::from_secs(1));
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), None) {
            AclPopOutcome::Resynced {
                dropped_prefix,
                declared,
                buffer_len_before,
                ..
            } => {
                assert_eq!(dropped_prefix, 4);
                assert_eq!(declared, 4 + 0xffff);
                assert_eq!(buffer_len_before, 4 + real.len());
            }
            other => panic!("expected Resynced: {:?}", other),
        }
        assert!(partial.is_none(), "resync clears partial_since");
        // After resync the real frame is at the head of the buffer.
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), None) {
            AclPopOutcome::Frame(pkt) => assert_eq!(pkt, real),
            other => panic!("expected Frame after resync: {:?}", other),
        }
    }

    #[test]
    fn pop_acl_frame_resyncs_past_map_body_ascii_garbage() {
        // ISSUES.md §"ACL accumulator: corrupt header declares NNNNN
        // bytes" — during long MAP message traffic the accumulator
        // occasionally ends up with 60–80 bytes of ASCII MAP body
        // prefix (e.g. `'ion='1.0' encoding='UTF-8' stand'`) in
        // front of a real ACL header. This regression test pins
        // the resync-and-recover path so a future refactor of the
        // sanity-MAX heuristic doesn't quietly break it.
        let garbage = b"ion='1.0' encoding='UTF-8' standalone='yes'?><MAP-msg-listing version='1";
        let mut buf: Vec<u8> = garbage.to_vec();
        // Active ACL handle 0x0042 — strict resync looks for an
        // ACL header carrying exactly this handle.
        let real = acl_frame(0x0042, &[0xde, 0xad, 0xbe, 0xef]);
        buf.extend_from_slice(&real);
        let mut partial = None;
        // Run with the active handle pinned (the live receptionist
        // path always knows the bonded ACL handle by the time MAP
        // traffic flows). The strict resync should anchor on
        // handle 0x0042 and drop exactly `garbage.len()` bytes.
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), Some(0x0042)) {
            AclPopOutcome::Resynced {
                dropped_prefix,
                declared,
                buffer_len_before,
                ..
            } => {
                assert_eq!(dropped_prefix, garbage.len());
                assert!(
                    declared > ACL_FRAME_SANITY_MAX,
                    "MAP body bytes interpreted as a length should always trip the sanity ceiling"
                );
                assert_eq!(buffer_len_before, garbage.len() + real.len());
            }
            other => panic!("expected Resynced for MAP-body garbage prefix: {:?}", other),
        }
        // After resync, the real ACL frame drains intact.
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), Some(0x0042)) {
            AclPopOutcome::Frame(pkt) => assert_eq!(pkt, real),
            other => panic!("expected Frame after MAP resync: {:?}", other),
        }
        assert!(
            buf.is_empty(),
            "buffer fully drained after resync + real frame"
        );
    }

    #[test]
    fn pop_acl_frame_flushes_when_no_resync_target_exists() {
        // Header trips sanity (declares 65 535 B); the rest of the
        // buffer is solid 0xff so handle 0x0fff > 0x0eff fails the
        // resync check at every offset → flush.
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        buf.extend(std::iter::repeat(0xffu8).take(40));
        let mut partial = None;
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), None) {
            AclPopOutcome::Flushed {
                reason: AclFlushReason::NoResyncTarget,
                declared,
                had,
                ..
            } => {
                assert_eq!(declared, 4 + 0xffff);
                assert_eq!(had, 44);
            }
            other => panic!("expected NoResyncTarget flush: {:?}", other),
        }
        assert!(buf.is_empty(), "flush clears the buffer");
        assert!(partial.is_none());
    }

    #[test]
    fn pop_acl_frame_flushes_after_partial_stall_timeout() {
        // 100-byte payload (104-byte total) but buffer holds only 50.
        // First call stamps partial_since; second call after the 3 s
        // timeout flushes with a PartialStall reason.
        let frame = acl_frame(0x0042, &[0xaa; 100]);
        let mut buf = frame[..50].to_vec();
        let mut partial = None;
        let t0 = Instant::now();
        match pop_acl_frame(&mut buf, &mut partial, t0, None) {
            AclPopOutcome::Partial { declared, had } => {
                assert_eq!(declared, 104);
                assert_eq!(had, 50);
            }
            other => panic!("expected Partial: {:?}", other),
        }
        assert_eq!(partial, Some(t0));
        // Advance past ACL_PARTIAL_TIMEOUT (3 s).
        let t1 = t0 + Duration::from_secs(4);
        match pop_acl_frame(&mut buf, &mut partial, t1, None) {
            AclPopOutcome::Flushed {
                reason: AclFlushReason::PartialStall(elapsed),
                declared,
                had,
                ..
            } => {
                assert!(elapsed >= Duration::from_secs(4));
                assert_eq!(declared, 104);
                assert_eq!(had, 50);
            }
            other => panic!("expected PartialStall flush: {:?}", other),
        }
        assert!(buf.is_empty(), "stall-flush clears the buffer");
        assert!(partial.is_none());
    }

    #[test]
    fn pop_acl_frame_keeps_partial_within_timeout() {
        // Same setup as the stall test — but advance only 1 s. The
        // partial-since stamp must NOT advance: the timer keeps
        // counting from the original first-hit moment so a slow
        // trickle eventually trips the wedge detector.
        let frame = acl_frame(0x0042, &[0xaa; 100]);
        let mut buf = frame[..50].to_vec();
        let mut partial = None;
        let t0 = Instant::now();
        let _ = pop_acl_frame(&mut buf, &mut partial, t0, None);
        assert_eq!(partial, Some(t0));
        let t1 = t0 + Duration::from_secs(1);
        match pop_acl_frame(&mut buf, &mut partial, t1, None) {
            AclPopOutcome::Partial { .. } => {}
            other => panic!("expected Partial within timeout: {:?}", other),
        }
        assert_eq!(
            partial,
            Some(t0),
            "partial_since must not advance — the wedge timer resets only on a successful drain",
        );
    }

    #[test]
    fn pop_acl_frame_treats_one_byte_short_as_partial_not_drain() {
        // Off-by-one fixture: declared length 4 (total 8) but buffer
        // holds 7. We must report Partial, not silently drain 7 bytes
        // (which would corrupt every subsequent frame).
        let frame = acl_frame(0x0042, &[0xaa, 0xbb, 0xcc, 0xdd]);
        assert_eq!(frame.len(), 8);
        let mut buf = frame[..7].to_vec();
        let mut partial = None;
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), None) {
            AclPopOutcome::Partial { declared, had } => {
                assert_eq!(declared, 8);
                assert_eq!(had, 7);
            }
            other => panic!("expected Partial for off-by-one short: {:?}", other),
        }
        assert_eq!(buf.len(), 7, "buffer untouched while partial");
    }

    #[test]
    fn pop_acl_frame_strict_handle_skips_false_positive_resync() {
        // Reproduces the Broadcom-21ec corruption pattern from R13:
        // a 1-byte garbage prefix sits in front of a fragment whose
        // first 4 bytes happen to look like a valid (but wrong-handle)
        // ACL header — handle 0x0006 length 64 — and the *real* header
        // for handle 0x000b sits 11 bytes deeper. The original loose
        // resync stopped at the false positive and stalled 3s on a
        // partial frame that would never complete; with the active
        // handle threaded in, the strict pass walks past the false
        // positive and lands on the real header.
        let mut buf: Vec<u8> = vec![
            0x80, // garbage prefix
            // Plausible-but-wrong header (handle 0x006 length 64)
            0x06, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0xbc, 0x7e,
            // Real ACL header for handle 0x000b length 24, payload 0..=23
            0x0b, 0x20, 0x18, 0x00, 0x14, 0x00, 0x40, 0x00, 0x06, 0x00, 0x00, 0x00, 0x0f, 0x35,
            0x03, 0x19, 0x12, 0x00,
        ];
        // Pad payload out to 24 bytes so the real frame is complete.
        while buf.len() < 4 + 11 + 24 {
            buf.push(0xee);
        }
        let mut partial = None;
        let outcome = pop_acl_frame(&mut buf, &mut partial, Instant::now(), Some(0x000b));
        match outcome {
            AclPopOutcome::Resynced { dropped_prefix, .. } => {
                assert_eq!(
                    dropped_prefix, 11,
                    "strict resync must walk past the handle-0x006 false positive \
                     and land on the real handle-0x000b header at offset 11"
                );
            }
            other => panic!("expected Resynced(11), got {:?}", other),
        }
    }

    #[test]
    fn pop_acl_frame_strict_handle_holds_buffer_when_real_header_not_yet_arrived() {
        // Same shape as the test above, but the buffer is short — the
        // real header hasn't been delivered by USB yet. With a known
        // active handle and no strict match, we must not flush (the
        // real bytes might still be in flight) and must not accept
        // the false-positive header at offset 1 either. Result: Partial,
        // and the partial-stall timer is now armed for the eventual
        // recovery flush 3s later.
        let mut buf: Vec<u8> = vec![
            0x80, 0x06, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0xbc, 0x7e,
        ];
        let mut partial = None;
        let outcome = pop_acl_frame(&mut buf, &mut partial, Instant::now(), Some(0x000b));
        match outcome {
            AclPopOutcome::Partial { .. } => {}
            other => panic!("expected Partial (wait for real header), got {:?}", other),
        }
        // Buffer must NOT be touched — the real header is presumed to
        // be in flight and will arrive on a subsequent read.
        assert_eq!(buf.len(), 11, "strict-miss must preserve the buffer");
        assert!(partial.is_some(), "partial-stall timer must be armed");
    }

    #[test]
    fn pop_acl_frame_falls_back_to_loose_resync_when_no_active_handle() {
        // When expected_handle is None (e.g. very early bring-up,
        // before any Connection_Complete event), the strict pass is
        // skipped and the loose criteria still fire. This is the
        // pre-R13 behavior; the strict path is purely additive.
        let mut buf: Vec<u8> = vec![
            // Offset 0: handle word 0x42aa has BC=1 (not 0) so the
            // header is flagged corrupt and we enter the resync path.
            0xaa, 0x42, 0x00, 0x40,
            // Offset 1 onwards: handle word 0x0042 (BC=0, handle 0x42),
            // length 0x0140 = 320 (≤ 1024 resync bound). Loose
            // criteria accept this.
            0x01, 0xbb, 0xcc, 0xdd,
        ];
        let mut partial = None;
        let outcome = pop_acl_frame(&mut buf, &mut partial, Instant::now(), None);
        match outcome {
            AclPopOutcome::Resynced { dropped_prefix, .. } => {
                assert_eq!(dropped_prefix, 1);
            }
            other => panic!("expected loose Resynced(1), got {:?}", other),
        }
    }

    /// R14-#8: long MAP message split across many adversarial ACL chunk
    /// boundaries. The OBEX body for a real Pixel SMS push easily blows
    /// past 1 KB (subject + sender + ISO 8601 timestamp + body + the
    /// envelope's BMessage header), and the WinUSB driver will hand
    /// those bytes back to userspace in arbitrary chunk sizes — see
    /// `project_winusb_acl_streaming`. The reassembler must never
    /// confuse OBEX body bytes for an ACL header, even when the
    /// payload happens to embed a 4-byte sequence that would look
    /// like one.
    ///
    /// Test layout:
    ///   - One 1500-byte ACL frame (1504 bytes on the wire) for handle
    ///     0x002a, simulating a long MAP NewMessage push.
    ///   - The payload embeds a "trap" sequence at offset 500 — the
    ///     bytes 0x42 0x00 0x10 0x00 form a syntactically valid ACL
    ///     header (handle 0x042, length 16). If the accumulator ever
    ///     read from a non-zero offset, it would resync to the trap
    ///     and silently drop ~500 bytes of body.
    ///   - Wire bytes are split at deliberately mean offsets:
    ///       * 2 bytes (mid-header)
    ///       * +1 byte (3 bytes total — still mid-header)
    ///       * +200 bytes (header complete, partial body)
    ///       * +298 bytes (lands one byte before the trap)
    ///       * +1 byte (now sitting on the trap's first byte)
    ///       * remainder (rest of body)
    ///   - After each push we expect either Partial or Frame, never
    ///     Resynced — the strict-handle path must keep us anchored to
    ///     the real ACL header at offset 0.
    #[test]
    fn pop_acl_frame_reassembles_long_map_message_through_adversarial_chunks() {
        const HANDLE: u16 = 0x002a;
        const PAYLOAD_LEN: usize = 1500;
        const TRAP_OFFSET: usize = 500;

        let mut payload = vec![0u8; PAYLOAD_LEN];
        // Fill with non-zero so a stray zero would stand out in a
        // post-mortem dump.
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = ((i % 251) + 1) as u8;
        }
        // The trap: a plausible ACL header for handle 0x042 length 16.
        // BC=0, PB=0, length 0x0010 little-endian.
        payload[TRAP_OFFSET] = 0x42;
        payload[TRAP_OFFSET + 1] = 0x00;
        payload[TRAP_OFFSET + 2] = 0x10;
        payload[TRAP_OFFSET + 3] = 0x00;

        let frame = acl_frame(HANDLE, &payload);
        assert_eq!(frame.len(), 4 + PAYLOAD_LEN);

        // Push boundaries chosen to be hostile: mid-header, just-
        // post-header, mid-payload, one-before-trap, on-the-trap.
        let split_points = [
            2,   // first 2 bytes of header
            3,   // 1 more byte (still mid-header)
            204, // header + 200 body bytes
            502, // 502 wire bytes total (= 1 byte before trap at body offset 500)
            503, // sitting on trap byte 1
            frame.len(),
        ];

        let mut buf: Vec<u8> = Vec::new();
        let mut partial: Option<Instant> = None;
        let mut prev = 0usize;
        for &cut in &split_points[..split_points.len() - 1] {
            buf.extend_from_slice(&frame[prev..cut]);
            prev = cut;
            // Each intermediate push must yield Partial — never Frame
            // (we don't have all the bytes yet) and never Resynced
            // (we're anchored to the real header at offset 0).
            match pop_acl_frame(&mut buf, &mut partial, Instant::now(), Some(HANDLE)) {
                AclPopOutcome::Partial { declared, had } => {
                    assert_eq!(declared, frame.len());
                    assert_eq!(had, cut, "had bytes should match cumulative push count");
                }
                AclPopOutcome::NotReady if cut < 4 => {
                    // <4-byte buffer can return NotReady — header isn't
                    // even readable yet. Only legal for the first cut.
                }
                other => panic!(
                    "intermediate push at {} bytes must stay Partial (anchored to real header), \
                     got {:?}",
                    cut, other,
                ),
            }
            assert_eq!(
                buf.len(),
                cut,
                "buffer must accumulate, not drain mid-frame"
            );
        }
        // Final push: deliver the remaining bytes. Now pop must yield
        // the complete frame — and the bytes returned must equal the
        // original wire frame, including the trap untouched at body
        // offset TRAP_OFFSET (= wire offset TRAP_OFFSET + 4).
        buf.extend_from_slice(&frame[prev..]);
        match pop_acl_frame(&mut buf, &mut partial, Instant::now(), Some(HANDLE)) {
            AclPopOutcome::Frame(pkt) => {
                assert_eq!(pkt.len(), frame.len(), "drained frame must be full length");
                assert_eq!(
                    &pkt[..],
                    &frame[..],
                    "drained frame must equal original wire bytes"
                );
                assert_eq!(
                    pkt[4 + TRAP_OFFSET],
                    0x42,
                    "trap byte must survive intact in the body — body bytes were never \
                     re-interpreted as a header",
                );
                assert_eq!(pkt[4 + TRAP_OFFSET + 1], 0x00);
                assert_eq!(pkt[4 + TRAP_OFFSET + 2], 0x10);
                assert_eq!(pkt[4 + TRAP_OFFSET + 3], 0x00);
            }
            other => panic!("final push must yield Frame, got {:?}", other),
        }
        assert!(buf.is_empty(), "drain must clear the buffer");
        assert!(
            partial.is_none(),
            "successful drain must clear the partial-stall timer"
        );
    }

    /// R4-#4 fuzz harness. The reviewer flagged ACL accumulator
    /// corruption during MAP traffic as "still real, even if masked"
    /// — the watchdog's 5s recover beat is a workaround, not a fix.
    /// We can't reproduce the underlying USB-streaming corruption
    /// without hardware, but we CAN strengthen the reassembler's
    /// robustness coverage so a future refactor that quietly breaks
    /// fragmented-frame handling fails CI before it ships.
    ///
    /// Strategy: run 200 trials. Each trial mints 3-5 ACL frames
    /// with random handles and payload lengths up to 2 KiB (typical
    /// MAP message size), concatenates them, then splits the wire
    /// stream into 1-50 random chunks. After each chunk push,
    /// `pop_acl_frame` is drained until it returns NotReady /
    /// Partial. The collected frames must match the originals
    /// byte-for-byte, in order, with no Resynced or Flushed
    /// outcomes (those would indicate spurious corruption-detection
    /// triggered by legitimate fragmentation).
    ///
    /// Pseudo-random seed is fixed so the test is deterministic on
    /// CI but exercises the full split-boundary surface.
    #[test]
    fn pop_acl_frame_fuzz_random_frames_random_splits() {
        // Tiny LCG so we don't pull in `rand` for a deterministic
        // test. Numerical Recipes constants — quality is fine for
        // boundary-shuffling test data, no security claim.
        struct Lcg(u64);
        impl Lcg {
            fn next_u32(&mut self) -> u32 {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (self.0 >> 32) as u32
            }
            fn next_in(&mut self, lo: u32, hi: u32) -> u32 {
                lo + self.next_u32() % (hi - lo + 1)
            }
        }
        let mut rng = Lcg(0x4f4b5f52345f5f34); // "OK_R4__4" — deterministic
        const TRIALS: usize = 200;
        for trial in 0..TRIALS {
            let n_frames = rng.next_in(3, 5) as usize;
            // Live receptionist always has one ACL connection at a
            // time per peer, so every frame shares one handle. Mint
            // it once per trial so the strict-resync path matches
            // every frame in the stream.
            let handle = (rng.next_in(0x0010, 0x00ff) & 0x0fff) as u16;
            let mut originals: Vec<Vec<u8>> = Vec::with_capacity(n_frames);
            let mut wire = Vec::new();
            for _ in 0..n_frames {
                // Bias toward MAP-sized payloads to exercise the
                // long-fragment path the reviewer flagged.
                let payload_len = rng.next_in(8, 2048) as usize;
                let mut payload = vec![0u8; payload_len];
                for (i, byte) in payload.iter_mut().enumerate() {
                    // Fill with a marker that lets us detect any
                    // mid-frame mutation post-drain.
                    *byte = (i ^ trial ^ handle as usize) as u8;
                }
                let frame = acl_frame(handle, &payload);
                originals.push(frame.clone());
                wire.extend_from_slice(&frame);
            }
            // Split into 1..=50 chunks at random offsets.
            let n_splits = rng.next_in(0, 49) as usize;
            let mut split_points: Vec<usize> = (0..n_splits)
                .map(|_| rng.next_in(1, wire.len() as u32 - 1) as usize)
                .collect();
            split_points.push(wire.len());
            split_points.sort_unstable();
            split_points.dedup();

            let mut buf: Vec<u8> = Vec::new();
            let mut partial = None;
            let mut drained: Vec<Vec<u8>> = Vec::new();
            let mut prev = 0usize;
            for cut in &split_points {
                buf.extend_from_slice(&wire[prev..*cut]);
                prev = *cut;
                // Drain everything the buffer currently has.
                loop {
                    match pop_acl_frame(&mut buf, &mut partial, Instant::now(), Some(handle)) {
                        AclPopOutcome::Frame(pkt) => drained.push(pkt),
                        AclPopOutcome::Partial { .. } | AclPopOutcome::NotReady => break,
                        // The reassembler must NOT spuriously trigger
                        // resync or flush on legitimate fragmentation.
                        other => panic!(
                            "trial {} unexpected outcome on legitimate ACL stream: {:?}",
                            trial, other
                        ),
                    }
                }
            }
            assert_eq!(
                drained.len(),
                originals.len(),
                "trial {}: drained {} frames, expected {}",
                trial,
                drained.len(),
                originals.len()
            );
            for (i, (got, want)) in drained.iter().zip(originals.iter()).enumerate() {
                assert_eq!(
                    got, want,
                    "trial {}: frame {} differs after fragmented reassembly",
                    trial, i
                );
            }
            assert!(
                buf.is_empty(),
                "trial {}: buffer must be fully drained at end of stream",
                trial
            );
            assert!(
                partial.is_none(),
                "trial {}: partial-stall timer must clear after the last frame",
                trial
            );
        }
    }

    #[test]
    fn first32_hex_pads_short_buffers_and_truncates_long_ones() {
        assert_eq!(first32_hex(&[]), "");
        assert_eq!(first32_hex(&[0x00]), "00");
        assert_eq!(first32_hex(&[0x42, 0x00, 0x03, 0x00]), "42 00 03 00");
        let long: Vec<u8> = (0..50).map(|i| i as u8).collect();
        let dump = first32_hex(&long);
        // Exactly 32 hex pairs separated by spaces.
        assert_eq!(dump.split(' ').count(), 32);
    }
}
