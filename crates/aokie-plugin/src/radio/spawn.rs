//! The `spawn` entry points (Windows radio thread / non-Windows stub) and the answer tone.

#[allow(unused_imports)]
use super::*;

// â”€â”€ Windows: the real radio â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Start the live radio on a background thread. Returns immediately with a
/// handle; initialisation happens asynchronously and is reflected in
/// [`RadioStatus`] (and via an `aokie.dongle.ready` / `aokie.hardware.error`
/// event). `preferred_path` pins a specific WinUSB dongle path, or `None`
/// takes the first enumerated HCI controller.
#[cfg(target_os = "windows")]
pub fn spawn(
    data_dir: std::path::PathBuf,
    preferred_path: Option<String>,
    auto_answer: bool,
    answer_tone: bool,
    reenumerate_hwid: Option<String>,
    greeting: Option<String>,
    ack_mode: bool,
    host_rpc: Arc<crate::host_rpc::HostRpc>,
    transport_mode: crate::backend::TransportMode,
) -> Result<RadioHandle, String> {
    use crate::backend::{RadioBackend, TransportMode};
    use std::sync::mpsc;

    let (control_tx, control_rx) = mpsc::channel::<RadioControl>();
    let status = Arc::new(RadioStatus::default());
    let status_thread = status.clone();
    let remote_media = crate::remote_media::RemoteMediaHandle::spawn()?;
    let remote_media_thread = remote_media.clone();

    std::thread::Builder::new()
        .name("aokie-plugin-radio".to_string())
        // Match the runtime thread's generous stack â€” the deep ACL â†’ L2CAP â†’
        // RFCOMM â†’ HFP dispatch overflowed the 1 MiB Windows default.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            // Transport selection (settings.transportMode): the WinUSB dongle
            // is the proven full-control backend; `native` uses the built-in
            // Windows Bluetooth stack (aokie-winbt) so no driver install is
            // needed; `auto` prefers native when a Windows adapter exists.
            let use_native = match transport_mode {
                TransportMode::Native => true,
                TransportMode::Auto => aokie_winbt::runtime::adapter_present(),
                TransportMode::Dongle => false,
            };
            // Software "virtual replug": on a cold boot the dongle's SCO iso
            // endpoint is dead until the device is re-enumerated (physically
            // unplug/replug). CM_Reenumerate the device before opening it so a
            // headless receptionist works after boot with no manual replug.
            // Best-effort + gated (settings.reenumerateHwid); settle briefly so
            // the device + WinUSB re-bind before we open it.
            if !use_native {
                if let Some(hwid) = reenumerate_hwid.as_deref() {
                match aokie_dongle::winusb::restart_device(hwid) {
                    Ok(()) => {
                        eprintln!("[aokie-plugin] restarted {hwid} (virtual replug: remove + re-add) â€” settling 3s");
                        std::thread::sleep(std::time::Duration::from_millis(3000));
                    }
                    Err(e) => eprintln!("[aokie-plugin] virtual replug {hwid} failed (continuing): {e}"),
                }
            }
            }
            // Raise this process's timer resolution to 1 ms for the lifetime of
            // the radio (see Cargo.toml note). The SCO iso path services USB
            // frames every 1 ms; at the ~15.6 ms per-process default a bare
            // plugin's read_sco waits + TX pacing are too coarse and every iso
            // transfer fails (empty + Win32 87). The original Tauri app gets
            // this for free via WebView2. timeBeginPeriod is ref-counted and
            // paired with timeEndPeriod below.
            unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
            let mut bt: Box<dyn RadioBackend> = if use_native {
                match crate::backend::NativeRadioBackend::start() {
                    Ok(b) => Box::new(b),
                    Err(e) => {
                        eprintln!("[aokie-plugin] radio failed to start (native backend): {e}");
                        *status_thread.last_error.lock().unwrap() = Some(e);
                        unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
                        return;
                    }
                }
            } else {
                match crate::backend::UsbRadioBackend::new(preferred_path) {
                    Ok(b) => Box::new(b),
                    Err(e) => {
                        eprintln!("[aokie-plugin] radio failed to start: {e}");
                        *status_thread.last_error.lock().unwrap() = Some(e);
                        unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
                        return;
                    }
                }
            };
            eprintln!(
                "[aokie-plugin] radio backend: {} (transportMode={})",
                bt.backend_name(),
                transport_mode.as_str()
            );
            // AOK-BT-001: publish the shared pairing window so phone.status can
            // report pairing state lock-free.
            *status_thread.pairing_window.lock().unwrap() = Some(bt.pairing_window());
            // PAIR-001: publish the shared pending-confirmation slot the same way.
            *status_thread.pairing_confirm.lock().unwrap() = Some(bt.pairing_confirm_slot());
            // Second connection to the same outbox file (see module docs).
            // Fail CLOSED (audit AOK-RUN-001): call/SMS events are the
            // business record — if they can't be durably queued, the radio
            // must not run and LOOK available while silently downgrading to
            // direct stdout. The error lands in last_error, so health reads
            // degraded and the operator sees why.
            let outbox = match Outbox::open(&data_dir.join(crate::connector::OUTBOX_FILE)) {
                Ok(o) => o,
                Err(e) => {
                    let msg = format!(
                        "radio outbox unavailable ({e}) — refusing to start without durable event delivery"
                    );
                    eprintln!("[aokie-plugin] {msg}");
                    *status_thread.last_error.lock().unwrap() = Some(msg);
                    unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
                    return;
                }
            };
            let mode = crate::event_bridge::EmitMode::for_host(ack_mode, crate::event_bridge::legacy_host_allowed());
            let mut sink = crate::event_bridge::StdoutSink::new();
            // Exit supervision (audit AOK-RUN-001): the spawner returned long
            // ago — if the loop panics or returns, readiness must flip
            // IMMEDIATELY, never leaving "initialized/connected" green on a
            // thread that no longer exists.
            let status_exit = status_thread.clone();
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_loop(
                    bt.as_mut(),
                    Some((&outbox, mode)),
                    &mut sink,
                    control_rx,
                    status_thread,
                    auto_answer,
                    answer_tone,
                    greeting,
                    host_rpc,
                    &data_dir,
                    remote_media_thread.clone(),
                );
            }));
            if ran.is_err() {
                let msg = "radio thread panicked — phone service stopped".to_string();
                eprintln!("[aokie-plugin] {msg}");
                *status_exit.last_error.lock().unwrap() = Some(msg);
            }
            status_exit.initialized.store(false, Ordering::Relaxed);
            status_exit.connected.store(false, Ordering::Relaxed);
            status_exit.call_active.store(false, Ordering::Relaxed);
            *status_exit.current_call_id.lock().unwrap() = None;
            remote_media_thread.observe_physical_call(None, false);
            unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
        })
        .map_err(|e| format!("spawn radio thread: {e}"))?;

    Ok(RadioHandle {
        control_tx,
        status,
        remote_media: Some(remote_media),
    })
}

/// A short two-note chime (mono i16 at the SCO sample rate) used to verify the
/// OUTBOUND SCO audio path actually reaches the caller on a given dongle â€” real
/// TTS speech replaces it once outbound audio is confirmed. Fades each note in
/// and out to avoid clicks.
#[cfg(target_os = "windows")]
pub(super) fn greeting_tone(sample_rate: u16) -> Vec<i16> {
    let sr = sample_rate.max(8000) as f32;
    let mut out = Vec::new();
    for &(freq, secs) in &[(660.0f32, 0.35f32), (880.0, 0.5)] {
        let n = (sr * secs) as usize;
        for i in 0..n {
            let t = i as f32 / sr;
            let env = ((i as f32 / n as f32) * std::f32::consts::PI).sin(); // 0â†’1â†’0
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * env * 0.6;
            out.push((s * i16::MAX as f32) as i16);
        }
    }
    out
}

// â”€â”€ Non-Windows: no radio â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[cfg(not(target_os = "windows"))]
pub fn spawn(
    _data_dir: std::path::PathBuf,
    _preferred_path: Option<String>,
    _auto_answer: bool,
    _answer_tone: bool,
    _reenumerate_hwid: Option<String>,
    _greeting: Option<String>,
    _ack_mode: bool,
    _transport_mode: crate::backend::TransportMode,
) -> Result<RadioHandle, String> {
    Err("the Aokie radio is only supported on Windows (WinUSB)".to_string())
}
