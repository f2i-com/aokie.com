//! `NativeBtRuntime` — the native Windows-stack radio engine.
//!
//! This is the concrete twin of `aokie_dongle::bluetooth::BluetoothManager`
//! for the built-in Windows Bluetooth stack (see
//! `docs/NATIVE_BLUETOOTH_TRANSPORT_PLAN.md`). It speaks the same event/audio
//! vocabulary (`BluetoothEvent` / `AudioData`) so the plugin's voice pipeline
//! runs unchanged on either transport.
//!
//! Phase 1 note: the full public API surface exists so the plugin's
//! `RadioBackend` seam compiles and runs against it; the worker currently
//! refuses controls with an honest error while the engine modules
//! (calls/audio/rfcomm/map/pairing) land in Phase 2.

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};

use aokie_bluetooth::aokie_radio::runtime::{PairingConfirmSlot, PairingWindow};
use aokie_dongle::bluetooth::{AudioData, BluetoothEvent};

/// Controls the plugin can queue to the native engine worker.
#[derive(Debug)]
pub(crate) enum NativeControl {
    Answer,
    RejectOrHangup,
    Dial(String),
    HoldSwap,
    QueryCalls,
    SendSms {
        message_id: String,
        recipient: String,
        body: String,
        msg_type: Option<String>,
        reply: Sender<Result<(), String>>,
    },
    Connect(String, Sender<Result<bool, String>>),
    Disconnect(String, Sender<Result<bool, String>>),
    RemovePaired(String, Sender<Result<bool, String>>),
    Shutdown,
}

/// Shared, cheap-to-clone view of the native engine's status (the native
/// analogue of the runtime's status atomics).
pub(crate) struct NativeShared {
    pub sample_rate: AtomicU16,
    pub connected: AtomicBool,
    /// True while a call is Talking (drives the WASAPI pump start/stop).
    pub call_active: AtomicBool,
    pub local_address: RwLock<String>,
    pub remote_address: RwLock<String>,
    /// Connected phone's numeric Bluetooth address (RFCOMM target).
    pub phone_address: RwLock<u64>,
    pub remote_name: RwLock<Option<String>>,
}

impl Default for NativeShared {
    fn default() -> Self {
        Self {
            sample_rate: AtomicU16::new(0),
            connected: AtomicBool::new(false),
            call_active: AtomicBool::new(false),
            local_address: RwLock::new(String::new()),
            remote_address: RwLock::new(String::new()),
            phone_address: RwLock::new(0),
            remote_name: RwLock::new(None),
        }
    }
}

/// TTS/caller PCM queued for the WASAPI render stream. The plugin pushes via
/// `send_audio`; the render thread drains at real time and feeds silence when
/// empty (keeps the link open, mirroring the dongle's TX keepalive).
pub(crate) struct TxAudio {
    q: std::sync::Mutex<std::collections::VecDeque<i16>>,
}

/// Matches the dongle runtime's 480k-sample SCO TX ring cap.
const TX_QUEUE_CAP: usize = 480_000;

impl TxAudio {
    fn new() -> Self {
        Self {
            q: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    pub(crate) fn push(&self, samples: &[i16]) -> bool {
        let Ok(mut q) = self.q.lock() else {
            return false;
        };
        q.extend(samples.iter().copied());
        if q.len() > TX_QUEUE_CAP {
            let drop = q.len() - TX_QUEUE_CAP;
            q.drain(..drop);
        }
        true
    }

    /// Take up to `max` samples; the caller silence-fills the remainder.
    pub(crate) fn drain(&self, max: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(max);
        if let Ok(mut q) = self.q.lock() {
            let take = q.len().min(max);
            out.extend(q.drain(..take));
        }
        out
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut q) = self.q.lock() {
            q.clear();
        }
    }
}

/// True when Windows sees a usable Bluetooth adapter (the native backend's
/// `auto` transport probe). Cheap and side-effect free.
pub fn adapter_present() -> bool {
    windows::Devices::Bluetooth::BluetoothAdapter::GetDefaultAsync()
        .and_then(|op| op.get())
        .is_ok()
}

pub struct NativeBtRuntime {
    event_rx: Receiver<BluetoothEvent>,
    audio_rx: Receiver<AudioData>,
    control_tx: Sender<NativeControl>,
    tx_audio: Arc<TxAudio>,
    shared: Arc<NativeShared>,
    pairing_window: PairingWindow,
    pairing_confirm: PairingConfirmSlot,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl NativeBtRuntime {
    /// Start the native engine on a background thread. Returns immediately —
    /// readiness surfaces as `Initialized` / `DeviceConnected` events (or a
    /// truthful `Error`) exactly like the WinUSB runtime.
    pub fn start() -> Result<Self, String> {
        let (event_tx, event_rx) = channel::<BluetoothEvent>();
        let (audio_tx, audio_rx) = channel::<AudioData>();
        let (control_tx, control_rx) = channel::<NativeControl>();
        let tx_audio = Arc::new(TxAudio::new());
        let shared = Arc::new(NativeShared::default());
        let pairing_window = PairingWindow::default();
        let pairing_confirm = PairingConfirmSlot::default();

        let worker_shared = shared.clone();
        let worker_window = pairing_window.clone();
        let worker_confirm = pairing_confirm.clone();
        let worker_tx_audio = tx_audio.clone();
        let worker = std::thread::Builder::new()
            .name("aokie-winbt".to_string())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                crate::worker::run(
                    event_tx,
                    audio_tx,
                    control_rx,
                    worker_tx_audio,
                    worker_shared,
                    worker_window,
                    worker_confirm,
                );
            })
            .map_err(|e| format!("spawn aokie-winbt worker: {e}"))?;

        Ok(Self {
            event_rx,
            audio_rx,
            control_tx,
            tx_audio,
            shared,
            pairing_window,
            pairing_confirm,
            worker: Some(worker),
        })
    }

    pub fn try_recv_event(&mut self) -> Option<BluetoothEvent> {
        self.event_rx.try_recv().ok()
    }

    pub fn try_recv_audio(&mut self) -> Option<AudioData> {
        self.audio_rx.try_recv().ok()
    }

    pub fn send_audio(&self, samples: &[i16]) -> bool {
        self.tx_audio.push(samples)
    }

    pub fn flush_tx_audio(&self) {
        self.tx_audio.clear();
    }

    pub fn send_sms(
        &self,
        message_id: String,
        recipient_phone: String,
        body: String,
        msg_type: Option<String>,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = channel();
        self.control_tx
            .send(NativeControl::SendSms {
                message_id,
                recipient: recipient_phone,
                body,
                msg_type,
                reply: reply_tx,
            })
            .map_err(|_| "native engine worker is gone".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "native engine worker dropped the SMS request".to_string())?
    }

    pub fn answer_call(&self) -> Result<(), String> {
        self.control_tx
            .send(NativeControl::Answer)
            .map_err(|_| "native engine worker is gone".to_string())
    }

    pub fn reject_call(&self) -> Result<(), String> {
        self.control_tx
            .send(NativeControl::RejectOrHangup)
            .map_err(|_| "native engine worker is gone".to_string())
    }

    pub fn hangup(&self) -> Result<(), String> {
        self.control_tx
            .send(NativeControl::RejectOrHangup)
            .map_err(|_| "native engine worker is gone".to_string())
    }

    pub fn dial(&self, number: String) -> Result<(), String> {
        self.control_tx
            .send(NativeControl::Dial(number))
            .map_err(|_| "native engine worker is gone".to_string())
    }

    pub fn hold_swap(&self) -> Result<(), String> {
        self.control_tx
            .send(NativeControl::HoldSwap)
            .map_err(|_| "native engine worker is gone".to_string())
    }

    pub fn query_calls(&self) -> Result<(), String> {
        self.control_tx
            .send(NativeControl::QueryCalls)
            .map_err(|_| "native engine worker is gone".to_string())
    }

    pub fn open_pairing_window(&self, seconds: u64) {
        self.pairing_window.open_for(seconds);
    }

    pub fn close_pairing_window(&self) {
        self.pairing_window.close();
    }

    pub fn pairing_window(&self) -> PairingWindow {
        self.pairing_window.clone()
    }

    pub fn pairing_confirm_slot(&self) -> PairingConfirmSlot {
        self.pairing_confirm.clone()
    }

    pub fn bonded_devices(&self) -> Vec<(String, Option<String>)> {
        crate::pairing::bonded_devices()
    }

    pub fn connected_name(&self) -> Option<String> {
        self.shared.remote_name.read().ok().and_then(|n| n.clone())
    }

    pub fn remove_paired(&self, address: &str) -> Result<bool, String> {
        let (reply_tx, reply_rx) = channel();
        self.control_tx
            .send(NativeControl::RemovePaired(address.to_string(), reply_tx))
            .map_err(|_| "native engine worker is gone".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "native engine worker dropped the unpair request".to_string())?
    }

    pub fn disconnect(&self, address: &str) -> Result<bool, String> {
        let (reply_tx, reply_rx) = channel();
        self.control_tx
            .send(NativeControl::Disconnect(address.to_string(), reply_tx))
            .map_err(|_| "native engine worker is gone".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "native engine worker dropped the disconnect request".to_string())?
    }

    pub fn connect(&self, address: &str) -> Result<bool, String> {
        let (reply_tx, reply_rx) = channel();
        self.control_tx
            .send(NativeControl::Connect(address.to_string(), reply_tx))
            .map_err(|_| "native engine worker is gone".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "native engine worker dropped the connect request".to_string())?
    }

    /// Windows' own pairing dialog owns SSP confirmation, so the operator
    /// confirm command has no handle to resolve in native mode.
    pub fn confirm_pairing(&self, _address: &str, _accept: bool) -> Result<(), String> {
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    pub fn get_sample_rate(&self) -> u16 {
        self.shared.sample_rate.load(Ordering::Acquire)
    }

    pub fn get_local_address(&self) -> String {
        self.shared
            .local_address
            .read()
            .map(|a| a.clone())
            .unwrap_or_default()
    }

    pub fn get_remote_address(&self) -> String {
        self.shared
            .remote_address
            .read()
            .map(|a| a.clone())
            .unwrap_or_default()
    }

    fn shutdown(&mut self) {
        let _ = self.control_tx.send(NativeControl::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for NativeBtRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}
