//! Offline speech endpoint check using the exact radio detector. Only generated
//! fixture audio is used; this does not access a microphone or telephone.
#[path = "../src/radio/capture_activity.rs"]
mod capture_activity;

fn main() {
    use aokie_plugin::speech_wire::{decode_wav_mono_i16, resample_i16_mono, encode_wav_pcm16_mono};
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::Path::new(args.get(1).expect("fixture directory"));
    let mut gate = capture_activity::CaptureActivity::default();
    let mut input = Vec::new();
    // Deterministic telephone background with RMS above the previous fixed gate.
    let mut seed = 23_u32;
    let mut noise = || { seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); ((seed >> 16) as i32 - 32768) as f32 * 0.025 };
    let mut speech_ends = Vec::new();
    for name in ["hello.wav", "response.wav"] {
        input.extend((0..8000).map(|_| noise() as i16));
        let wav = decode_wav_mono_i16(&std::fs::read(dir.join(name)).unwrap()).unwrap();
        let pcm = resample_i16_mono(&wav.samples, wav.sample_rate, 8000);
        input.extend(pcm.iter().map(|&v| (v as f32 * 0.55 + noise()).clamp(-32768.0,32767.0) as i16));
        speech_ends.push(input.len());
        input.extend((0..8000).map(|_| noise() as i16));
    }
    let mut utterance = Vec::new();
    let mut active = false;
    let mut silence_ms = 0;
    let mut endings = Vec::new();
    for (index, frame) in input.chunks(80).enumerate() {
        let f16 = aokie_plugin::voice::to_f32_16k(frame,8000);
        if gate.detect(frame,&f16,active) {
            if !active { gate.prepend_onset(&mut utterance); }
            active = true;silence_ms = 0;utterance.extend(&f16);
        } else if active {
            silence_ms += 10;utterance.extend(&f16);
        } else { gate.remember_quiet(&f16); }
        if active && silence_ms >= 450 {
            let pcm: Vec<i16> = utterance.drain(..).map(|x: f32| (x*32768.0) as i16).collect();
            endings.push((index+1)*80);
            std::fs::write(dir.join(format!("detected-{}.wav",endings.len())),encode_wav_pcm16_mono(&pcm,16000)).unwrap();
            active=false;silence_ms=0;
        }
    }
    assert_eq!(endings.len(),2,"two speech turns must finish despite continuous background noise");
    for (end, speech_end) in endings.iter().zip(speech_ends) {
        assert!(*end <= speech_end + 8000,"reply must not wait more than a second after fixture speech");
    }
    println!("PASS: two telephone-band speech turns ended promptly over continuous noise; WAVs contain generated test speech only.");
}
