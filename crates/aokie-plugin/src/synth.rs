//! Speech-synthesis worker (voice feature): synthesis runs OFF the radio
//! thread.
//!
//! Phase 2 of the full-duplex plan. Before this, `tts_speak` synthesized on
//! the radio thread and queued WHOLE sentences into the SCO TX queue — so a
//! cancel had to wait out local Pocket-TTS's ~1.2 s decode chunks, the AEC
//! reference could lead real playout by seconds (and overflow its 3 s FIFO on
//! long sentences), and the mic went undrained during synthesis stalls.
//!
//! Now ONE worker thread owns the TTS engines (in-process Pocket-TTS and the
//! HTTP endpoint with its sticky per-call fallback) and streams small PCM
//! blocks — already rated (WSOLA) and resampled to the SCO rate — over a
//! bounded channel. The radio thread paces those blocks into the SCO TX
//! queue (~200 ms ahead of playout) while it keeps draining the mic; see
//! `radio::tts_speak`.
//!
//! Cancellation is an EPOCH: [`SynthHandle::begin`] bumps a shared counter
//! and every output block is tagged with the epoch it belongs to. Bumping
//! the epoch again ([`SynthHandle::cancel`]) makes the worker abort at its
//! next chunk boundary and the radio side discard anything stale — audio
//! stops within the ~200 ms the TX queue holds, never a whole sentence.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use crate::radio::HttpTtsRuntime;

/// One job for the worker. `Span` carries the epoch it was minted under —
/// the worker skips it if the epoch has already moved on.
enum SynthJob {
    Span {
        epoch: u64,
        text: String,
        voice: String,
        rate: f32,
        target_rate: u32,
    },
    Configure {
        endpoint: Option<String>,
    },
    ResetCall,
}

/// Worker → radio: epoch-tagged PCM blocks and the span's terminal outcome.
pub enum SynthOut {
    Frames { epoch: u64, pcm: Vec<i16> },
    /// Synthesis finished (all frames sent). Also sent after an epoch abort —
    /// the radio discards it as stale.
    Done { epoch: u64 },
    Failed { epoch: u64, error: String },
}

/// Block size the worker slices output into (~240 ms at the SCO rate): small
/// enough that backpressure and epoch aborts are responsive, large enough
/// that channel overhead is negligible.
fn block_samples(target_rate: u32) -> usize {
    (target_rate as usize / 4).max(160)
}

/// Bounded output-channel depth in blocks (~10 s of audio): synthesis may run
/// ahead of playout, but never unboundedly.
const OUT_CHANNEL_BLOCKS: usize = 40;

pub struct SynthHandle {
    job_tx: mpsc::Sender<SynthJob>,
    pub out_rx: mpsc::Receiver<SynthOut>,
    epoch: Arc<AtomicU64>,
}

impl SynthHandle {
    /// Spawn the worker. The HTTP TTS endpoint is seeded from
    /// `AOKIE_TTS_ENDPOINT` exactly as the old in-loop runtime was.
    pub fn spawn() -> Self {
        let (job_tx, job_rx) = mpsc::channel::<SynthJob>();
        let (out_tx, out_rx) = mpsc::sync_channel::<SynthOut>(OUT_CHANNEL_BLOCKS);
        let epoch = Arc::new(AtomicU64::new(0));
        let worker_epoch = epoch.clone();
        let spawned = std::thread::Builder::new()
            .name("aokie-tts-synth".to_string())
            .spawn(move || worker(job_rx, out_tx, worker_epoch));
        if let Err(e) = spawned {
            // The radio side degrades naturally: begin() jobs go nowhere and
            // every span reads as a synthesis failure (dur 0 → TTS error slot).
            eprintln!("[aokie-plugin] TTS synth worker failed to start: {e}");
        }
        Self {
            job_tx,
            out_rx,
            epoch,
        }
    }

    /// Start synthesizing one span; returns its epoch. Implicitly cancels
    /// whatever came before (the epoch moves) and drains stale output so the
    /// caller starts clean — this also unblocks a worker parked on a full
    /// channel from an abandoned span.
    pub fn begin(&self, text: &str, voice: &str, rate: f32, target_rate: u32) -> u64 {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        while self.out_rx.try_recv().is_ok() {}
        let _ = self.job_tx.send(SynthJob::Span {
            epoch,
            text: text.to_string(),
            voice: voice.to_string(),
            rate,
            target_rate,
        });
        epoch
    }

    /// Abort the current span: the worker stops at its next chunk boundary
    /// and everything still in flight reads as stale.
    pub fn cancel(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Live TTS-endpoint reconfiguration (settings.set / flow push).
    pub fn configure(&self, endpoint: Option<String>) {
        let _ = self.job_tx.send(SynthJob::Configure { endpoint });
    }

    /// Call boundary: clear the HTTP endpoint's sticky per-call fallback and
    /// invalidate any in-flight span.
    pub fn reset_call(&self) {
        self.cancel();
        let _ = self.job_tx.send(SynthJob::ResetCall);
        while self.out_rx.try_recv().is_ok() {}
    }
}

fn worker(
    job_rx: mpsc::Receiver<SynthJob>,
    out_tx: mpsc::SyncSender<SynthOut>,
    epoch: Arc<AtomicU64>,
) {
    let mut tts: Option<crate::voice::TtsEngine> = None;
    let mut http = HttpTtsRuntime::from_env("AOKIE_TTS_ENDPOINT");
    while let Ok(job) = job_rx.recv() {
        match job {
            SynthJob::Configure { endpoint } => http.configure(endpoint),
            SynthJob::ResetCall => http.reset_call(),
            SynthJob::Span {
                epoch: e,
                text,
                voice,
                rate,
                target_rate,
            } => {
                if epoch.load(Ordering::SeqCst) != e {
                    continue; // superseded before we even started
                }
                match synth_span(&mut tts, &mut http, &text, &voice, rate, target_rate, &epoch, e, &out_tx) {
                    Ok(()) => {
                        let _ = out_tx.send(SynthOut::Done { epoch: e });
                    }
                    Err(err) => {
                        let _ = out_tx.send(SynthOut::Failed { epoch: e, error: err });
                    }
                }
            }
        }
    }
}

/// Slice `pcm` into bounded blocks and ship them, aborting between blocks
/// when the epoch moves on. Returns false when aborted.
fn ship_blocks(
    pcm: &[i16],
    target_rate: u32,
    epoch: &AtomicU64,
    e: u64,
    out_tx: &mpsc::SyncSender<SynthOut>,
) -> bool {
    for block in pcm.chunks(block_samples(target_rate)) {
        if epoch.load(Ordering::SeqCst) != e {
            return false;
        }
        if out_tx
            .send(SynthOut::Frames {
                epoch: e,
                pcm: block.to_vec(),
            })
            .is_err()
        {
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn synth_span(
    tts: &mut Option<crate::voice::TtsEngine>,
    http: &mut HttpTtsRuntime,
    text: &str,
    voice: &str,
    rate: f32,
    target_rate: u32,
    epoch: &AtomicU64,
    e: u64,
    out_tx: &mpsc::SyncSender<SynthOut>,
) -> Result<(), String> {
    let rated = (aokie_core::time_stretch::clamp_rate(rate) - 1.0).abs() >= 0.01;
    // HTTP endpoint first (sticky per-call fallback semantics unchanged).
    if let Some(endpoint) = http.endpoint_for_call() {
        match crate::radio::http_tts_synthesize_at(&endpoint, text, voice) {
            Ok(wav) => {
                // Stretch at the provider's native rate (full quality), then
                // resample to SCO. Applied HERE, never asked of the provider,
                // so every endpoint behaves identically.
                let samples = if rated {
                    aokie_core::time_stretch::stretch_i16(&wav.samples, wav.sample_rate, rate)
                } else {
                    wav.samples
                };
                let pcm =
                    crate::speech_wire::resample_i16_mono(&samples, wav.sample_rate, target_rate);
                ship_blocks(&pcm, target_rate, epoch, e, out_tx);
                return Ok(());
            }
            Err(err) => {
                if http.mark_failed_for_call() {
                    eprintln!(
                        "[aokie-plugin] HTTP TTS failed at {endpoint}: {err}; falling back to in-process TTS for this call"
                    );
                }
            }
        }
    }
    if tts.is_none() {
        match crate::voice::TtsEngine::load() {
            Ok(engine) => {
                eprintln!("[aokie-plugin] TTS engine loaded");
                *tts = Some(engine);
            }
            Err(err) => return Err(format!("TTS load failed: {err}")),
        }
    }
    let engine = tts.as_mut().ok_or_else(|| "TTS engine unavailable".to_string())?;
    if rated {
        // Rated spans are short (a slowed phone number): synthesize whole,
        // stretch at the model's native rate, then ship.
        let (native_pcm, native_rate) = engine
            .synthesize_native(text, voice)
            .map_err(|err| format!("TTS synthesis failed: {err}"))?;
        let stretched = aokie_core::time_stretch::stretch_i16(&native_pcm, native_rate, rate);
        let pcm = crate::speech_wire::resample_i16_mono(&stretched, native_rate, target_rate);
        ship_blocks(&pcm, target_rate, epoch, e, out_tx);
        return Ok(());
    }
    // Streaming path: ship each synthesized chunk as it lands so the caller
    // hears the first ~0.3 s chunk while the rest still decodes.
    engine
        .synthesize_streaming(text, voice, target_rate, |pcm| {
            ship_blocks(pcm, target_rate, epoch, e, out_tx)
        })
        .map_err(|err| format!("TTS synthesis failed: {err}"))?;
    Ok(())
}
