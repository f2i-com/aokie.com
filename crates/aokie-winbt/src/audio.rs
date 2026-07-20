//! WASAPI Hands-Free audio pump — the native backend's SCO replacement.
//!
//! Windows' built-in HFP service owns the phone's SCO/eSCO link; the only
//! supported way to move call audio is the pair of "Bluetooth Hands-Free"
//! WASAPI endpoints (render = to the caller, capture = from the caller), and
//! merely STARTING those streams makes Windows open the SCO link on demand
//! (plan: `docs/NATIVE_BLUETOOTH_TRANSPORT_PLAN.md` §109). This module owns
//! one MTA COM thread that:
//!
//!   1. waits for `shared.call_active` (set by the calls engine),
//!   2. opens render+capture streams on the HF endpoints — retrying every
//!      500 ms because the endpoints appear a second or two AFTER the answer,
//!   3. pumps capture → 16 kHz mono i16 → `audio_tx` in 20 ms chunks and
//!      `tx_audio` → render in 10 ms chunks (silence-padded, mirroring the
//!      dongle's TX keepalive so an underfed link cannot drop),
//!   4. tears everything down when the call ends (and on stop/Drop).
//!
//! Format contract with the plugin: ALWAYS 16 kHz mono i16 either way,
//! exactly like the WinUSB mSBC path. Preferred route is WASAPI's own
//! converter (AUTOCONVERTPCM); the fallback converts manually (f32/i16,
//! downmix, linear-resample) — all of which is pure Rust and unit-tested
//! below without touching audio hardware.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aokie_dongle::bluetooth::{AudioData, BluetoothEvent};
use windows::core::GUID;
use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_DeviceInterface_FriendlyName, PKEY_Device_DeviceDesc,
};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eCapture, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};

use crate::runtime::{NativeShared, TxAudio};

/// The one rate the plugin voice pipeline speaks (matches the mSBC path).
const OUT_RATE: u32 = 16_000;
const OUT_RATE_U16: u16 = OUT_RATE as u16;
/// 20 ms capture chunks — the same frame size the dongle pipeline consumes.
const CAPTURE_CHUNK: usize = 320;
/// 10 ms render slices — cadence of the dongle's SCO TX keepalive.
const RENDER_CHUNK: usize = 160;
const LOOP_TICK: Duration = Duration::from_millis(5);
const RENDER_PERIOD: Duration = Duration::from_millis(10);
/// HF endpoints appear a second or two after the call answers (Windows opens
/// SCO on demand); retry quietly instead of erroring the call out.
const OPEN_RETRY: Duration = Duration::from_millis(500);
/// Shared-mode buffer, 200 ms in 100 ns units. Big enough to absorb scheduler
/// jitter on a BT link; the latency cost is accepted for v1 (plan flags
/// WASAPI latency as a barge-feel tuning item, not a correctness one).
const BUFFER_DURATION_HNS: i64 = 2_000_000;
/// 0x80010106 — thread already COM-initialized with the OTHER apartment
/// model. Harmless for WASAPI polling use; just don't CoUninitialize (we did
/// not add a reference in that case).
const RPC_E_CHANGED_MODE: i32 = -2_147_417_850;
/// Format tags not re-exported under the enabled windows-crate features
/// (KernelStreaming/Multimedia modules are off; values are ABI constants).
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Owns the pump thread. Dropped by the worker (Phase 2 wiring) at runtime
/// shutdown; `stop()` is the explicit early-off switch for transport swaps.
///
/// `#[allow(dead_code)]` until `worker.rs` wires the pump in Phase 2 — the
/// crate compiles with stubs and nothing constructs it yet.
#[allow(dead_code)]
pub(crate) struct AudioPump {
    stop: Arc<AtomicBool>,
    /// Mutex so `stop(&self)` and `Drop` share exactly one join.
    thread: Mutex<Option<JoinHandle<()>>>,
}

#[allow(dead_code)] // see struct note
impl AudioPump {
    pub(crate) fn start(
        event_tx: Sender<BluetoothEvent>,
        audio_tx: Sender<AudioData>,
        tx_audio: Arc<TxAudio>,
        shared: Arc<NativeShared>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let thread = std::thread::Builder::new()
            .name("aokie-winbt-audio".to_string())
            .stack_size(1024 * 1024)
            .spawn(move || pump_loop(event_tx, audio_tx, tx_audio, shared, stop_thread))
            .map_err(|e| format!("spawn aokie-winbt audio pump: {e}"))?;
        Ok(Self {
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut guard) = self.thread.lock() {
            if let Some(handle) = guard.take() {
                // The loop sleeps at most LOOP_TICK, so the join is bounded;
                // a wedged WASAPI call would only stall shutdown, never the
                // radio thread (the pump owns no radio locks).
                let _ = handle.join();
            }
        }
    }
}

impl Drop for AudioPump {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Both directions of one call's audio, plus teardown that never leaves the
/// shared sample rate lying (the plugin gates ALL speech on sr > 0).
struct Streams {
    render: OpenedStream,
    capture: OpenedStream,
}

fn close_streams(streams: &mut Option<Streams>) {
    if let Some(s) = streams.take() {
        // SAFETY: clients are live COM objects owned by this thread; Stop is
        // idempotent and errors only matter for diagnostics we don't have yet.
        unsafe {
            let _ = s.render.client.Stop();
            let _ = s.capture.client.Stop();
        }
    }
}

/// The pump loop's state machine:
///   DOWN + call_active -> open (retry 500 ms) -> UP + AudioConnected
///   UP + !call_active  -> close -> DOWN + AudioDisconnected
///   UP + stream error  -> close -> DOWN + AudioDisconnected -> reopen retry
fn pump_loop(
    event_tx: Sender<BluetoothEvent>,
    audio_tx: Sender<AudioData>,
    tx_audio: Arc<TxAudio>,
    shared: Arc<NativeShared>,
    stop: Arc<AtomicBool>,
) {
    // This thread touches COM; initialize it ourselves. RPC_E_CHANGED_MODE
    // means someone already set the other model — WASAPI polling works from
    // either apartment, so carry on.
    // SAFETY: called once per thread; balanced by CoUninitialize below only
    // when this call actually took a reference (S_OK/S_FALSE).
    let com_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com_hr.is_err() && com_hr.0 != RPC_E_CHANGED_MODE {
        let _ = event_tx.send(BluetoothEvent::Error(format!(
            "native audio pump: COM init failed 0x{:08X}",
            com_hr.0
        )));
        return;
    }
    let com_ours = com_hr.0 >= 0;

    let mut streams: Option<Streams> = None;
    let mut pending_mono: Vec<i16> = Vec::with_capacity(CAPTURE_CHUNK * 4);
    let mut last_open_attempt: Option<Instant> = None; // None = first try immediate
    let mut last_render = Instant::now();

    'main: while !stop.load(Ordering::Acquire) {
        let call_active = shared.call_active.load(Ordering::Acquire);

        if call_active
            && streams.is_none()
            && last_open_attempt.is_none_or(|t| t.elapsed() >= OPEN_RETRY)
        {
            last_open_attempt = Some(Instant::now());
            match open_streams() {
                Ok((s, codec)) => {
                    shared.sample_rate.store(OUT_RATE_U16, Ordering::Release);
                    // Never let one call's tail bleed into the next call's
                    // fresh stream (the dongle's stale-audio-leak lesson).
                    pending_mono.clear();
                    last_render = Instant::now();
                    streams = Some(s);
                    let _ = event_tx.send(BluetoothEvent::AudioConnected {
                        codec,
                        sample_rate: OUT_RATE_U16,
                        // Streams opened = SCO open on Windows; there is no
                        // separate arming step to fail here.
                        armed: true,
                    });
                }
                // Endpoint not up yet (or genuinely absent) — the loop
                // retries; no error event, this is the expected answer lag.
                Err(_why) => {}
            }
        } else if !call_active && streams.is_some() {
            close_streams(&mut streams);
            pending_mono.clear();
            shared.sample_rate.store(0, Ordering::Release);
            let _ = event_tx.send(BluetoothEvent::AudioDisconnected);
        }

        let mut stream_dead = false;
        if let Some(s) = streams.as_mut() {
            // CAPTURE: drain every queued packet, then ship 20 ms chunks.
            if pump_capture(&mut s.capture, &mut pending_mono).is_err() {
                stream_dead = true;
            } else {
                while pending_mono.len() >= CAPTURE_CHUNK {
                    let chunk: Vec<i16> = pending_mono.drain(..CAPTURE_CHUNK).collect();
                    if audio_tx
                        .send(AudioData {
                            samples: chunk,
                            sample_rate: OUT_RATE_U16,
                        })
                        .is_err()
                    {
                        // Plugin side is gone — nothing left to pump for.
                        break 'main;
                    }
                }
                // RENDER: feed the link every ~10 ms, silence when the TTS
                // queue is empty (keeps the HF link alive mid-pause).
                if last_render.elapsed() >= RENDER_PERIOD {
                    last_render = Instant::now();
                    if pump_render(&mut s.render, &tx_audio).is_err() {
                        stream_dead = true;
                    }
                }
            }
        }
        if stream_dead {
            // Device invalidated / endpoint vanished mid-call. Report the drop
            // truthfully; the open-retry above re-establishes while the call
            // stays active. (No Error event: a flap loop would spam it.)
            close_streams(&mut streams);
            pending_mono.clear();
            shared.sample_rate.store(0, Ordering::Release);
            let _ = event_tx.send(BluetoothEvent::AudioDisconnected);
            continue;
        }

        std::thread::sleep(LOOP_TICK);
    }

    close_streams(&mut streams);
    shared.sample_rate.store(0, Ordering::Release);
    if com_ours {
        // SAFETY: balances the successful CoInitializeEx above, on the same
        // thread, after all COM objects (the streams) are dropped.
        unsafe { CoUninitialize() };
    }
}

// ---------------------------------------------------------------------------
// Endpoint discovery + stream open
// ---------------------------------------------------------------------------

fn open_streams() -> Result<(Streams, String), String> {
    // SAFETY: pump_loop initialized COM on this thread before calling.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("MMDeviceEnumerator: {}", hr(&e)))?;
    // v1 = SINGLE PHONE: the first Hands-Free endpoint per direction wins.
    // Multi-phone selection (matching the BTHHF device id to
    // shared.remote_address) is deferred — the dongle backend has the same
    // single-call shape, so nothing upstream can exercise it yet.
    let render_dev = find_hf_endpoint(&enumerator, eRender)
        .ok_or_else(|| "no Hands-Free render endpoint".to_string())?;
    let capture_dev = find_hf_endpoint(&enumerator, eCapture)
        .ok_or_else(|| "no Hands-Free capture endpoint".to_string())?;
    let render = open_stream(&render_dev, true)?;
    let capture = match open_stream(&capture_dev, false) {
        Ok(c) => c,
        Err(e) => {
            // SAFETY: live client owned by this thread; best-effort Stop.
            unsafe {
                let _ = render.client.Stop();
            }
            return Err(e);
        }
    };
    // The endpoint's native mix rate is the codec hint: Windows negotiates
    // mSBC/WBS at 16 kHz (since Win10 1703); anything else means the link
    // fell back to narrowband. We still REPORT 16 kHz to the plugin because
    // the conversion is ours — this label is diagnostics, not a format claim.
    let codec = if render.mix_rate == OUT_RATE {
        "native-wbs".to_string()
    } else {
        "native-nbs".to_string()
    };
    Ok((Streams { render, capture }, codec))
}

fn find_hf_endpoint(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Option<IMMDevice> {
    // SAFETY: enumerator is live on this thread; returned devices are used
    // only here (COM MTA).
    let coll = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }.ok()?;
    let count = unsafe { coll.GetCount() }.ok()?;
    for i in 0..count {
        let dev = match unsafe { coll.Item(i) } {
            Ok(d) => d,
            Err(_) => continue,
        };
        let id = device_id(&dev);
        let name = read_prop(&dev, &PKEY_DeviceInterface_FriendlyName)
            .or_else(|| read_prop(&dev, &PKEY_Device_DeviceDesc))
            .unwrap_or_default();
        if looks_like_hands_free(&name, &id) {
            return Some(dev);
        }
    }
    None
}

/// HF endpoints name themselves "… Hands-Free …" and their device id carries
/// the BTHHF service class; "headset" is the fallback label some radios use.
/// Pure string matching (case-insensitive) so it is unit-testable.
fn looks_like_hands_free(name: &str, id: &str) -> bool {
    let name = name.to_lowercase();
    let id = id.to_lowercase();
    name.contains("hands-free")
        || id.contains("hands-free")
        || id.contains("bthhf")
        || name.contains("headset")
}

fn device_id(dev: &IMMDevice) -> String {
    // SAFETY: GetId's PWSTR is CoTaskMem-allocated by WASAPI; to_string
    // copies out, then we free the original exactly once.
    unsafe {
        let Ok(id) = dev.GetId() else {
            return String::new();
        };
        let s = id.to_string().unwrap_or_default();
        if !id.is_null() {
            CoTaskMemFree(Some(id.as_ptr() as *const core::ffi::c_void));
        }
        s
    }
}

/// Working pattern lifted from probe.rs (read_prop), plus the two frees the
/// docs require (the probe intentionally leaks a little for a one-shot run;
/// this runs per call, so it must not).
fn read_prop(dev: &IMMDevice, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: dev is live on this thread. The PROPVARIANT and the PWSTR from
    // PropVariantToStringAlloc are both released before returning.
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ).ok()?;
        let mut pv = store.GetValue(key).ok()?;
        let name = PropVariantToStringAlloc(std::ptr::from_ref(&pv)).ok();
        let s = name.and_then(|p| {
            let s = p.to_string().unwrap_or_default();
            if !p.is_null() {
                CoTaskMemFree(Some(p.as_ptr() as *const core::ffi::c_void));
            }
            Some(s)
        });
        let _ = PropVariantClear(&mut pv);
        s
    }
}

// ---------------------------------------------------------------------------
// Stream open / format negotiation
// ---------------------------------------------------------------------------

enum Service {
    Render(IAudioRenderClient),
    Capture(IAudioCaptureClient),
}

struct OpenedStream {
    client: IAudioClient,
    service: Service,
    /// Engine buffer size in (mix-format) frames — the padding ceiling.
    buffer_frames: u32,
    /// Endpoint's native mix rate (codec hint + fallback conversion rate).
    mix_rate: u32,
    mode: StreamMode,
}

enum StreamMode {
    /// WASAPI accepted our 16 kHz mono i16 format with AUTOCONVERTPCM —
    /// buffers are already in the plugin's format, zero work per packet.
    Direct16k,
    /// Manual conversion from/to the endpoint mix format.
    Convert(ConvertSpec),
}

struct ConvertSpec {
    rate: u32,
    channels: usize,
    is_float: bool,
    /// Streaming resampler (render: 16k→rate, capture: rate→16k) — stateful
    /// so packet seams don't click.
    resampler: LinearResampler,
}

/// The format the whole pipeline speaks — asked for first on every endpoint.
fn pcm_16k_mono() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 1,
        nSamplesPerSec: OUT_RATE,
        nAvgBytesPerSec: OUT_RATE * 2,
        nBlockAlign: 2,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

fn open_stream(dev: &IMMDevice, render: bool) -> Result<OpenedStream, String> {
    // SAFETY: dev is a live endpoint on this COM-initialized thread. The
    // GetMixFormat pointer is CoTaskMem-freed on every path below, after the
    // (possible) fallback Initialize that still borrows it.
    unsafe {
        let client: IAudioClient = dev
            .Activate::<IAudioClient>(CLSCTX_ALL, None)
            .map_err(|e| format!("activate IAudioClient: {}", hr(&e)))?;
        let mix = client
            .GetMixFormat()
            .map_err(|e| format!("GetMixFormat: {}", hr(&e)))?;
        // WAVEFORMATEX is packed(1): value-read the fields (no references).
        let (tag, channels, rate, bits, align) = {
            let f = &*mix;
            (
                f.wFormatTag,
                f.nChannels,
                f.nSamplesPerSec,
                f.wBitsPerSample,
                f.nBlockAlign,
            )
        };
        let prefer = pcm_16k_mono();
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let preferred_ok = client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                BUFFER_DURATION_HNS,
                0,
                &prefer,
                None,
            )
            .is_ok();
        let mode = if preferred_ok {
            CoTaskMemFree(Some(mix as *const core::ffi::c_void));
            StreamMode::Direct16k
        } else {
            let Some(spec) = convert_spec(mix, tag, channels, rate, bits, align, render) else {
                CoTaskMemFree(Some(mix as *const core::ffi::c_void));
                return Err(format!(
                    "unsupported HF mix format (tag {tag}, {bits}-bit, {channels} ch)"
                ));
            };
            let fallback = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                BUFFER_DURATION_HNS,
                0,
                mix,
                None,
            );
            CoTaskMemFree(Some(mix as *const core::ffi::c_void));
            fallback.map_err(|e| format!("Initialize (mix format): {}", hr(&e)))?;
            StreamMode::Convert(spec)
        };
        let buffer_frames = client
            .GetBufferSize()
            .map_err(|e| format!("GetBufferSize: {}", hr(&e)))?;
        let service = if render {
            Service::Render(
                client
                    .GetService::<IAudioRenderClient>()
                    .map_err(|e| format!("render service: {}", hr(&e)))?,
            )
        } else {
            Service::Capture(
                client
                    .GetService::<IAudioCaptureClient>()
                    .map_err(|e| format!("capture service: {}", hr(&e)))?,
            )
        };
        client.Start().map_err(|e| format!("Start: {}", hr(&e)))?;
        Ok(OpenedStream {
            client,
            service,
            buffer_frames,
            mix_rate: rate,
            mode,
        })
    }
}

/// Build the manual-conversion spec for the fallback path, or None when the
/// mix format is outside what we convert (f32 or i16, packed interleaved).
/// None fails the open; the 500 ms retry keeps polling — an endpoint that
/// never converts yields honest silence-free absence, not garbage audio.
fn convert_spec(
    mix: *const WAVEFORMATEX,
    tag: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    align: u16,
    render: bool,
) -> Option<ConvertSpec> {
    let is_float = if tag == WAVE_FORMAT_PCM as u16 {
        false
    } else if tag == WAVE_FORMAT_IEEE_FLOAT_TAG {
        true
    } else if tag == WAVE_FORMAT_EXTENSIBLE_TAG {
        // The buffer is really a WAVEFORMATEXTENSIBLE; its SubFormat GUID
        // decides PCM vs float. SAFETY: `mix` is a live GetMixFormat pointer
        // (freed by the caller AFTER this returns); WAVEFORMATEXTENSIBLE is
        // packed(1) so the GUID is value-read, never referenced.
        let sub = unsafe { (*(mix as *const WAVEFORMATEXTENSIBLE)).SubFormat };
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            true
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            false
        } else {
            return None;
        }
    } else {
        return None;
    };
    if channels == 0 || rate == 0 {
        return None;
    }
    if is_float && bits != 32 {
        return None;
    }
    if !is_float && bits != 16 {
        return None;
    }
    // Packed interleaved is the only layout the converters below read; a
    // padded block align would silently mis-index frames.
    if align != channels * (bits / 8) {
        return None;
    }
    Some(ConvertSpec {
        rate,
        channels: channels as usize,
        is_float,
        resampler: if render {
            LinearResampler::new(OUT_RATE, rate)
        } else {
            LinearResampler::new(rate, OUT_RATE)
        },
    })
}

// ---------------------------------------------------------------------------
// Per-tick pumping
// ---------------------------------------------------------------------------

fn pump_capture(stream: &mut OpenedStream, pending: &mut Vec<i16>) -> Result<(), String> {
    let Service::Capture(cap) = &stream.service else {
        return Ok(());
    };
    loop {
        // SAFETY: cap is live. The GetBuffer pointer is valid for
        // `frames` frames until ReleaseBuffer — every read below finishes
        // before the matching ReleaseBuffer, and the pointer never escapes.
        unsafe {
            let packet = cap
                .GetNextPacketSize()
                .map_err(|e| format!("GetNextPacketSize: {}", hr(&e)))?;
            if packet == 0 {
                return Ok(());
            }
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            cap.GetBuffer(&mut ptr, &mut frames, &mut flags, None, None)
                .map_err(|e| format!("capture GetBuffer: {}", hr(&e)))?;
            let n = frames as usize;
            let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            match &mut stream.mode {
                StreamMode::Direct16k => {
                    if silent {
                        // SILENT = buffer contents undefined — emit zeros,
                        // never the garbage bytes.
                        pending.resize(pending.len() + n, 0);
                    } else {
                        let src = std::slice::from_raw_parts(ptr as *const i16, n);
                        pending.extend_from_slice(src);
                    }
                }
                StreamMode::Convert(spec) => {
                    let mono: Vec<i16> = if silent {
                        // Silence at the mix rate; the resampler keeps phase.
                        vec![0; n]
                    } else if spec.is_float {
                        let src = std::slice::from_raw_parts(ptr as *const f32, n * spec.channels);
                        floats_to_mono_i16(src, spec.channels)
                    } else {
                        let src = std::slice::from_raw_parts(ptr as *const i16, n * spec.channels);
                        i16s_to_mono_i16(src, spec.channels)
                    };
                    let out = spec.resampler.process(&mono);
                    pending.extend_from_slice(&out);
                }
            }
            cap.ReleaseBuffer(frames)
                .map_err(|e| format!("capture ReleaseBuffer: {}", hr(&e)))?;
        }
    }
}

fn pump_render(stream: &mut OpenedStream, tx_audio: &TxAudio) -> Result<(), String> {
    let Service::Render(rend) = &stream.service else {
        return Ok(());
    };
    // SAFETY: rend/client are live. Each GetBuffer pointer is written for
    // exactly the released frame count and never held across ReleaseBuffer.
    unsafe {
        let padding = stream
            .client
            .GetCurrentPadding()
            .map_err(|e| format!("GetCurrentPadding: {}", hr(&e)))?;
        match &mut stream.mode {
            StreamMode::Direct16k => {
                // Frames == samples here (16 kHz mono). Padding check first:
                // asking for more frames than are free fails the write.
                let want = render_take(RENDER_CHUNK, stream.buffer_frames, padding);
                if want == 0 {
                    return Ok(());
                }
                let samples = pad_with_silence(tx_audio.drain(want), want);
                let ptr = rend
                    .GetBuffer(want as u32)
                    .map_err(|e| format!("render GetBuffer: {}", hr(&e)))?;
                let dst = std::slice::from_raw_parts_mut(ptr as *mut i16, want);
                dst.copy_from_slice(&samples);
                rend.ReleaseBuffer(want as u32, 0)
                    .map_err(|e| format!("render ReleaseBuffer: {}", hr(&e)))?;
            }
            StreamMode::Convert(spec) => {
                let available = stream.buffer_frames.saturating_sub(padding) as usize;
                // Budget in 16 kHz input samples with headroom so the
                // resampler's <=+1 output-frame rounding can never request
                // more frames than are free.
                let want_16k = render_take_16k_convert(available, spec.rate).min(RENDER_CHUNK);
                if want_16k == 0 {
                    return Ok(());
                }
                let padded = pad_with_silence(tx_audio.drain(want_16k), want_16k);
                let at_mix = spec.resampler.process(&padded);
                if at_mix.is_empty() {
                    return Ok(());
                }
                let n = at_mix.len();
                let ptr = rend
                    .GetBuffer(n as u32)
                    .map_err(|e| format!("render GetBuffer: {}", hr(&e)))?;
                if spec.is_float {
                    let dst = std::slice::from_raw_parts_mut(ptr as *mut f32, n * spec.channels);
                    dst.copy_from_slice(&mono_i16_to_float_frames(&at_mix, spec.channels));
                } else {
                    let dst = std::slice::from_raw_parts_mut(ptr as *mut i16, n * spec.channels);
                    dst.copy_from_slice(&mono_i16_to_i16_frames(&at_mix, spec.channels));
                }
                rend.ReleaseBuffer(n as u32, 0)
                    .map_err(|e| format!("render ReleaseBuffer: {}", hr(&e)))?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested below; no hardware touched)
// ---------------------------------------------------------------------------

/// Frames safe to write this tick in Direct16k mode: never more than the
/// queue can supply at the render cadence, never more than the free space.
fn render_take(requested: usize, buffer_frames: u32, padding: u32) -> usize {
    requested.min(buffer_frames.saturating_sub(padding) as usize)
}

/// 16 kHz input budget for the Convert render path. Two frames of headroom
/// keep the resampler's rounding (<= +1 output frame per block) strictly
/// below `available`, so GetBuffer can never be asked for more than is free.
fn render_take_16k_convert(available_frames: usize, mix_rate: u32) -> usize {
    if mix_rate == 0 {
        return 0;
    }
    let head = available_frames.saturating_sub(2) as u64;
    ((head * OUT_RATE as u64) / mix_rate as u64) as usize
}

/// Top the drained samples up to `len` with zeros — the render stream must
/// be fed at real time even when the TTS queue is empty (the dongle's TX
/// keepalive: an underfed HF link can drop mid-call).
fn pad_with_silence(mut samples: Vec<i16>, len: usize) -> Vec<i16> {
    samples.resize(len, 0);
    samples
}

/// Streaming linear-interpolation resampler between mono i16 blocks. Cheap
/// and phase-correct ACROSS blocks (the previous block's last sample and the
/// fractional output position are carried), so 10/20 ms packet seams don't
/// click — the whole point over a stateless per-packet resample.
struct LinearResampler {
    step: f64,     // source samples advanced per output frame = src/dst
    next_pos: f64, // global source position (input-sample units) of next output
    base: u64,     // global index of the current block's first input sample
    prev: i16,     // previous block's last sample (interpolation over the seam)
    has_prev: bool,
}

impl LinearResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            step: src_rate as f64 / dst_rate.max(1) as f64,
            next_pos: 0.0,
            base: 0,
            prev: 0,
            has_prev: false,
        }
    }

    fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        for (j, &cur) in input.iter().enumerate() {
            let cur_pos = self.base as f64 + j as f64;
            // Emit every output whose global source position falls inside the
            // segment (prev, cur]; frac in (0, 1] so frac == 1 lands exactly
            // ON cur (integer positions reproduce the input sample-for-sample
            // — identity and exact decimation stay lossless).
            while self.next_pos <= cur_pos {
                let frac = self.next_pos - (cur_pos - 1.0);
                let a = if j == 0 {
                    if self.has_prev {
                        self.prev
                    } else {
                        cur
                    }
                } else {
                    input[j - 1]
                };
                out.push(lerp_i16(a, cur, frac));
                self.next_pos += self.step;
            }
        }
        if let Some(&last) = input.last() {
            self.prev = last;
            self.has_prev = true;
            self.base += input.len() as u64;
        }
        out
    }
}

fn lerp_i16(a: i16, b: i16, frac: f64) -> i16 {
    let v = a as f64 + (b as f64 - a as f64) * frac;
    // Clamp despite the inputs being in range: f64 rounding at the extremes
    // can otherwise wrap i16 (worst case a full-scale click).
    v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

/// One-shot resample of a mono block to 16 kHz. Stateless convenience form —
/// the streaming paths use `LinearResampler` so block seams don't click; kept
/// for tests and one-shot callers.
#[allow(dead_code)]
fn resample_to_16k(mono: &[i16], src_rate: u32) -> Vec<i16> {
    if src_rate == 0 || src_rate == OUT_RATE {
        return mono.to_vec();
    }
    LinearResampler::new(src_rate, OUT_RATE).process(mono)
}

/// WASAPI float audio is full-scale [-1, 1]; scale by 2^15 and clamp — never
/// wrap. NaN (defensive; shouldn't occur) saturates to 0 via Rust's `as`.
fn f32_to_i16(x: f32) -> i16 {
    (x * 32768.0).clamp(-32768.0, 32767.0) as i16
}

fn i16_to_f32(x: i16) -> f32 {
    x as f32 / 32768.0
}

/// Interleaved float frames -> mono i16 (channel average, integer domain so
/// full-scale multichannel can't overflow mid-sum).
fn floats_to_mono_i16(frames: &[f32], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        frames.iter().map(|&x| f32_to_i16(x)).collect()
    } else {
        frames
            .chunks_exact(channels)
            .map(|ch| {
                let sum: i32 = ch.iter().map(|&x| f32_to_i16(x) as i32).sum();
                (sum / ch.len() as i32) as i16
            })
            .collect()
    }
}

/// Interleaved i16 frames -> mono i16 (channel average via i32 — a direct
/// i16 sum of two full-scale channels would wrap).
fn i16s_to_mono_i16(frames: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        frames.to_vec()
    } else {
        frames
            .chunks_exact(channels)
            .map(|ch| {
                let sum: i32 = ch.iter().map(|&x| x as i32).sum();
                (sum / ch.len() as i32) as i16
            })
            .collect()
    }
}

/// Mono i16 -> interleaved float frames (every channel gets the same sample —
/// the HF mic/earpiece is mono, replication beats panning maths).
fn mono_i16_to_float_frames(mono: &[i16], channels: usize) -> Vec<f32> {
    let ch = channels.max(1);
    let mut out = Vec::with_capacity(mono.len() * ch);
    for &s in mono {
        let f = i16_to_f32(s);
        for _ in 0..ch {
            out.push(f);
        }
    }
    out
}

fn mono_i16_to_i16_frames(mono: &[i16], channels: usize) -> Vec<i16> {
    let ch = channels.max(1);
    let mut out = Vec::with_capacity(mono.len() * ch);
    for &s in mono {
        for _ in 0..ch {
            out.push(s);
        }
    }
    out
}

fn hr(e: &windows::core::Error) -> String {
    format!("0x{:08X} {}", e.code().0, e.message())
}

// ---------------------------------------------------------------------------
// Tests — pure DSP/matching only; none of this touches audio hardware.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_identity_is_lossless() {
        let v: Vec<i16> = (-100..100).collect();
        assert_eq!(resample_to_16k(&v, 16_000), v);
        // Rate 0 is garbage input — pass through unchanged, never divide by 0.
        assert_eq!(resample_to_16k(&v, 0), v);
    }

    #[test]
    fn resample_empty_input() {
        assert!(resample_to_16k(&[], 48_000).is_empty());
        assert!(LinearResampler::new(48_000, 16_000).process(&[]).is_empty());
    }

    #[test]
    fn resample_48k_to_16k_decimates_exactly() {
        // 3:1 with integer output positions: every third sample, no blur.
        let input: Vec<i16> = (0..48).collect();
        let out = resample_to_16k(&input, 48_000);
        let want: Vec<i16> = (0..16).map(|k| k * 3).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn resample_8k_to_16k_interpolates_midpoints() {
        let out = resample_to_16k(&[0, 100, 200, 300], 8_000);
        assert_eq!(out, vec![0, 50, 100, 150, 200, 250, 300]);
    }

    #[test]
    fn resample_streaming_matches_one_shot() {
        // Deterministic non-periodic signal; odd block sizes stress the seam.
        let signal: Vec<i16> = (0..997u32)
            .map(|i| ((i * 137 + 31) % 4000) as i16 - 2000)
            .collect();
        let one_shot = resample_to_16k(&signal, 44_100);
        let mut r = LinearResampler::new(44_100, 16_000);
        let mut streamed = r.process(&signal[..73]);
        streamed.extend(r.process(&signal[73..323]));
        streamed.extend(r.process(&signal[323..]));
        assert_eq!(one_shot, streamed);
    }

    #[test]
    fn resample_constant_signal_is_invariant() {
        // A flat mono signal must stay exactly flat at any ratio — no
        // invented harmonics, no off-by-one drift at block ends.
        let down = resample_to_16k(&[1000; 64], 48_000);
        assert_eq!(down.len(), 22); // positions 0,3,...,63
        assert!(down.iter().all(|&s| s == 1000));
        let up = resample_to_16k(&[1000; 10], 8_000);
        assert_eq!(up.len(), 19); // positions 0,0.5,...,9
        assert!(up.iter().all(|&s| s == 1000));
    }

    #[test]
    fn resample_output_stays_in_bounds() {
        let extreme: Vec<i16> = (0..200)
            .map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        for rate in [8_000, 44_100, 48_000] {
            let out = resample_to_16k(&extreme, rate);
            assert!(out.iter().all(|&s| s >= i16::MIN && s <= i16::MAX));
        }
    }

    #[test]
    fn resample_44k1_ratio_and_tracking() {
        // Ramp: output j should track source position j * (44100/16000).
        let input: Vec<i16> = (0..1000).collect();
        let out = resample_to_16k(&input, 44_100);
        let step = 44_100.0 / 16_000.0;
        assert!((out.len() as f64 - 1000.0 / step).abs() <= 1.5);
        for (j, &s) in out.iter().enumerate() {
            assert!((s as f64 - j as f64 * step).abs() <= 2.0, "j={j} s={s}");
        }
    }

    #[test]
    fn f32_to_i16_scales_and_clamps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), i16::MIN);
        assert_eq!(f32_to_i16(0.5), 16384);
        // Out-of-range must clamp, never wrap.
        assert_eq!(f32_to_i16(5.0), i16::MAX);
        assert_eq!(f32_to_i16(-5.0), i16::MIN);
        assert_eq!(f32_to_i16(f32::NAN), 0);
    }

    #[test]
    fn floats_stereo_to_mono_averages() {
        assert_eq!(floats_to_mono_i16(&[1.0, -1.0], 2), vec![0]);
        assert_eq!(floats_to_mono_i16(&[0.5, 0.25], 2), vec![12288]);
        // Mono (channels=1) passes through sample-for-sample.
        assert_eq!(floats_to_mono_i16(&[0.0, 1.0], 1), vec![0, i16::MAX]);
    }

    #[test]
    fn i16_stereo_to_mono_averages_without_overflow() {
        assert_eq!(i16s_to_mono_i16(&[1000, -1000], 2), vec![0]);
        // Full-scale pair: naive i16 sum would wrap; i32 accumulation doesn't.
        assert_eq!(i16s_to_mono_i16(&[30_000, 30_000], 2), vec![30_000]);
        assert_eq!(i16s_to_mono_i16(&[1, 2, 3], 1), vec![1, 2, 3]);
    }

    #[test]
    fn mono_to_frames_replicates_channels() {
        assert_eq!(mono_i16_to_i16_frames(&[1, 2], 2), vec![1, 1, 2, 2]);
        let f = mono_i16_to_float_frames(&[i16::MAX], 2);
        assert_eq!(f.len(), 2);
        assert!((f[0] - 32767.0 / 32768.0).abs() < 1e-7);
    }

    #[test]
    fn pad_with_silence_tops_up() {
        assert_eq!(pad_with_silence(vec![1, 2, 3], 5), vec![1, 2, 3, 0, 0]);
        assert_eq!(pad_with_silence(vec![1, 2], 2), vec![1, 2]);
        assert_eq!(pad_with_silence(vec![], 3), vec![0, 0, 0]);
    }

    #[test]
    fn render_take_respects_padding() {
        assert_eq!(render_take(160, 3200, 0), 160);
        assert_eq!(render_take(160, 3200, 3120), 80);
        // Full buffer (padding == size): nothing may be written.
        assert_eq!(render_take(160, 3200, 3200), 0);
        // Degenerate: padding beyond size (racy driver) saturates to 0.
        assert_eq!(render_take(160, 3200, 9999), 0);
    }

    #[test]
    fn convert_budget_never_overruns_buffer() {
        // The load-bearing invariant: budgeted input -> resampled output must
        // never exceed the free frame count, or GetBuffer fails mid-call.
        for rate in [8_000u32, 16_000, 44_100, 48_000] {
            for available in [0usize, 1, 2, 3, 10, 160, 500, 3200] {
                let budget = render_take_16k_convert(available, rate).min(RENDER_CHUNK);
                let out = LinearResampler::new(16_000, rate).process(&vec![0i16; budget]);
                assert!(
                    out.len() <= available,
                    "rate={rate} available={available} budget={budget} out={}",
                    out.len()
                );
            }
        }
        assert_eq!(render_take_16k_convert(100, 0), 0); // rate 0: no divide
    }

    #[test]
    fn hands_free_matching() {
        assert!(looks_like_hands_free(
            "Headset (Pixel 8 Hands-Free)",
            "anything"
        ));
        assert!(looks_like_hands_free("whatever", "{guid}#BTHHF#9&1234"));
        assert!(looks_like_hands_free("", "SWD\\BTHHF\\123"));
        assert!(looks_like_hands_free("Jabra Headset", "usb"));
        assert!(!looks_like_hands_free("Speakers (Realtek)", "HDAUDIO"));
        assert!(!looks_like_hands_free("Microphone Array", "USB\\VID"));
    }
}
