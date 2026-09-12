//! Offline diagnostic for synthetic audio. Does not connect to a phone or record calls.
//! Usage: cargo run -p aokie-plugin --features voice --example inspect_speech_capture -- caller-8k.wav reference-8k.wav output-directory

#[cfg(feature = "voice")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use aokie_plugin::{aec::EchoCanceller, speech_wire, voice};
    use std::{fs, path::PathBuf};
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("expected caller WAV, reference WAV and output directory".into());
    }
    let caller = speech_wire::decode_wav_mono_i16(&fs::read(&args[1])?)?;
    let reference = speech_wire::decode_wav_mono_i16(&fs::read(&args[2])?)?;
    if caller.sample_rate != 8000 || reference.sample_rate != 8000 {
        return Err("both inputs must be mono 8 kHz PCM WAV".into());
    }
    let output = PathBuf::from(&args[3]);
    fs::create_dir_all(&output)?;
    let mut quiet_aec = EchoCanceller::new(8000);
    let mut overlap_aec = EchoCanceller::new(8000);
    let mut quiet = Vec::new();
    let mut overlap = Vec::new();
    // Match CVSD's 3 ms capture chunks. Reference has an artificial 80 ms
    // echo delay and 15% gain; this is a reproducible simulation, not a claim
    // about the actual Pixel's acoustic path.
    let mut padded = caller.samples.clone();
    padded.extend(std::iter::repeat(0).take(4000));
    for (chunk, speech) in padded.chunks(24).enumerate() {
        let offset = chunk * 24;
        let outgoing: Vec<i16> = (0..speech.len())
            .map(|i| reference.samples.get(offset + i).copied().unwrap_or(0))
            .collect();
        let mixed: Vec<i16> = speech.iter().enumerate().map(|(i, &s)| {
            let echo = (offset + i).checked_sub(640)
                .and_then(|j| reference.samples.get(j)).copied().unwrap_or(0);
            (s as f32 + echo as f32 * 0.15).clamp(-32768.0, 32767.0) as i16
        }).collect();
        quiet.extend(quiet_aec.process_capture(speech));
        overlap_aec.feed_reference(&outgoing);
        overlap.extend(overlap_aec.process_capture(&mixed));
    }
    for (name, samples) in [("narrowband", &padded), ("aec-quiet", &quiet), ("aec-overlap", &overlap)] {
        // Use the same resampler and PCM conversion as the HTTP STT lane.
        let pcm: Vec<i16> = voice::to_f32_16k(samples, 8000).into_iter()
            .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
        fs::write(output.join(format!("{name}.wav")), speech_wire::encode_wav_pcm16_mono(&pcm, 16000))?;
        println!("{name}: {} samples, RMS {:.0}", samples.len(), voice::frame_rms(samples));
    }
    Ok(())
}

#[cfg(not(feature = "voice"))]
fn main() {
    eprintln!("This diagnostic requires --features voice.");
    std::process::exit(1);
}
