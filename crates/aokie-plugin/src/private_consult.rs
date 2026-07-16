//! Bounded, isolated voice consultation with an authorised Companion user.
//!
//! This worker never owns Bluetooth and never receives SCO. Its only input is
//! [`RemoteMediaHandle::try_recv_consult_pcm`], whose binding is independently
//! checked against the current active consult lease. Its only output is
//! [`RemoteMediaHandle::try_push_consult_output`], which targets that same
//! WebRTC peer and has no caller/SCO route.

use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::assistance::VoiceConsultRequest;
use crate::remote_media::{RemoteMediaHandle, ServiceMode};

const COMMAND_CAPACITY: usize = 2;
const SPEECH_RMS: f32 = 350.0;
const ENDPOINT_SILENCE: Duration = Duration::from_millis(700);
const MIN_SPEECH: Duration = Duration::from_millis(300);
const MAX_ANSWER: Duration = Duration::from_secs(30);
const MAX_CONSULT: Duration = Duration::from_secs(120);
const OUTPUT_RATE: u32 = aokie_media::MEDIA_SAMPLE_RATE_HZ;

enum Command {
    Start {
        request: VoiceConsultRequest,
        device_id: String,
    },
    Stop,
}

pub struct PrivateConsultHandle {
    tx: SyncSender<Command>,
}

impl PrivateConsultHandle {
    pub fn spawn(media: RemoteMediaHandle) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        std::thread::Builder::new()
            .name("aokie-private-consult".into())
            .spawn(move || {
                while let Ok(command) = rx.recv() {
                    match command {
                        Command::Start { request, device_id } => {
                            if let Err(error) = run_session(&media, &request, &device_id) {
                                eprintln!(
                                    "[aokie-plugin] private consultation ended safely: {error}"
                                );
                                let _ = media.end_active_consult("consult_pipeline_failed");
                            }
                        }
                        Command::Stop => break,
                    }
                }
            })
            .map_err(|error| format!("spawn private consult worker: {error}"))?;
        Ok(Self { tx })
    }

    pub fn start(&self, request: VoiceConsultRequest, device_id: String) -> Result<(), String> {
        self.tx
            .try_send(Command::Start { request, device_id })
            .map_err(|_| "private consultation worker is busy".to_string())
    }
}

impl Drop for PrivateConsultHandle {
    fn drop(&mut self) {
        let _ = self.tx.try_send(Command::Stop);
    }
}

fn run_session(
    media: &RemoteMediaHandle,
    request: &VoiceConsultRequest,
    device_id: &str,
) -> Result<(), String> {
    let binding = media
        .active_consult_binding()
        .ok_or_else(|| "active consult binding disappeared".to_string())?;
    if binding.device_id != device_id
        || binding.call_id != request.fence.call_id
        || binding.call_epoch != request.fence.call_epoch
        || binding.owner_epoch != request.fence.owner_epoch
    {
        return Err("active consult does not match its assistance fence".into());
    }
    let hard_deadline = Instant::now() + MAX_CONSULT;
    let expiry_deadline = request
        .expires_at
        .saturating_sub(unix_now()?)
        .min(MAX_CONSULT.as_secs());
    if expiry_deadline == 0 {
        return Err("assistance request expired before consultation".into());
    }
    let deadline = Instant::now() + Duration::from_secs(expiry_deadline);

    // Load both halves before asking for an answer. If isolation can be
    // established but Aokie cannot hear or speak privately, fail closed and
    // return the caller instead of opening an inert microphone session.
    let mut tts = crate::voice::TtsEngine::load()
        .map_err(|error| format!("private TTS unavailable: {error}"))?;
    let mut stt = crate::voice::SttEngine::load()
        .map_err(|error| format!("private STT unavailable: {error}"))?;

    let prompt = format!(
        "Aokie here. I need your help with this: {} Please give me the answer for the caller.",
        request.question.trim()
    );
    speak(media, &mut tts, &prompt, deadline.min(hard_deadline))?;

    // Drop microphone frames captured while Aokie's prompt was playing. This
    // prevents speaker echo from becoming the authorised answer even on a
    // platform whose acoustic echo canceller is still converging.
    while media.try_recv_consult_pcm().is_some() {}
    std::thread::sleep(Duration::from_millis(250));
    while media.try_recv_consult_pcm().is_some() {}

    let answer_pcm = collect_one_answer(media, deadline.min(hard_deadline))?;
    let answer = stt
        .transcribe(&crate::voice::to_f32_16k(&answer_pcm, OUTPUT_RATE))
        .map_err(|error| format!("private answer transcription failed: {error}"))?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Err("private consultation contained no intelligible answer".into());
    }

    crate::assistance::global().accept_voice(
        &request.request_id,
        &request.fence,
        device_id,
        answer,
    )?;
    let _ = speak(
        media,
        &mut tts,
        "Thank you. I have the answer and will return to the caller now.",
        deadline.min(hard_deadline),
    );
    media.end_active_consult("consult_answer_received")
}

fn collect_one_answer(media: &RemoteMediaHandle, deadline: Instant) -> Result<Vec<i16>, String> {
    let started = Instant::now();
    let mut speech_started: Option<Instant> = None;
    let mut last_speech: Option<Instant> = None;
    let mut pcm = Vec::new();
    while Instant::now() < deadline && started.elapsed() < MAX_ANSWER {
        let snapshot = media.snapshot();
        if snapshot.service_mode != ServiceMode::ConsultActive || !snapshot.consent.consult_enabled
        {
            return Err("private consultation lease was revoked".into());
        }
        let Some(frame) = media.try_recv_consult_pcm() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let mono =
            crate::remote_media::resample_mono(&frame.samples, frame.sample_rate, OUTPUT_RATE);
        let now = Instant::now();
        if crate::voice::frame_rms(&mono) >= SPEECH_RMS {
            speech_started.get_or_insert(now);
            last_speech = Some(now);
        }
        if speech_started.is_some() {
            pcm.extend_from_slice(&mono);
        }
        if let (Some(first), Some(last)) = (speech_started, last_speech) {
            if now.duration_since(last) >= ENDPOINT_SILENCE
                && now.duration_since(first) >= MIN_SPEECH
            {
                break;
            }
        }
        // A hard memory bound even if a hostile endpoint publishes constant
        // energy and never yields silence.
        pcm.truncate((OUTPUT_RATE as usize) * MAX_ANSWER.as_secs() as usize);
    }
    let minimum = (OUTPUT_RATE as usize * MIN_SPEECH.as_millis() as usize) / 1_000;
    if pcm.len() < minimum {
        Err("private consultation timed out without an answer".into())
    } else {
        Ok(pcm)
    }
}

fn speak(
    media: &RemoteMediaHandle,
    tts: &mut crate::voice::TtsEngine,
    text: &str,
    deadline: Instant,
) -> Result<(), String> {
    let pcm = tts.synthesize(text, "", OUTPUT_RATE)?;
    // 20 ms chunks keep the peer packetizer responsive and bound the amount
    // of private audio queued after lease revocation.
    let chunk = (OUTPUT_RATE as usize / 50).max(1);
    for samples in pcm.chunks(chunk) {
        if Instant::now() >= deadline
            || media.snapshot().service_mode != ServiceMode::ConsultActive
            || !media.try_push_consult_output(samples, OUTPUT_RATE)
        {
            return Err("private consult output route closed".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before the Unix epoch".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consult_limits_are_bounded() {
        assert!(MIN_SPEECH < ENDPOINT_SILENCE);
        assert!(MAX_ANSWER < MAX_CONSULT);
        assert!(MAX_CONSULT.as_secs() <= 300);
    }
}
