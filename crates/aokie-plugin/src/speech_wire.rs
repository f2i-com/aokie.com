//! Small wire-format helpers shared by the optional HTTP speech engines.
//!
//! Kept dependency-free so the default plugin build can compile and test the
//! parsing logic without enabling the heavy `voice` feature.

pub use aokie_core::speech::normalize_speech_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WavPcm {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

/// Encode mono PCM16 into a minimal RIFF/WAVE file.
pub fn encode_wav_pcm16_mono(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = samples.len().saturating_mul(2) as u32;
    let riff_len = 36u32.saturating_add(data_len);
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate.saturating_mul(2)).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Decode RIFF/WAVE PCM into mono i16. PCM16 and IEEE-float32 are accepted;
/// multi-channel input is averaged to mono.
pub fn decode_wav_mono_i16(bytes: &[u8]) -> Result<WavPcm, String> {
    if bytes.len() < 12 {
        return Err("wav is too short".to_string());
    }
    if &bytes[0..4] != b"RIFF" {
        return Err("wav is not RIFF little-endian".to_string());
    }
    if &bytes[8..12] != b"WAVE" {
        return Err("wav is missing WAVE header".to_string());
    }

    let mut fmt: Option<WavFmt> = None;
    let mut data: Option<&[u8]> = None;
    let mut off = 12usize;
    while off + 8 <= bytes.len() {
        let id = &bytes[off..off + 4];
        let size = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        let start = off + 8;
        let end = start
            .checked_add(size)
            .ok_or_else(|| "wav chunk size overflows".to_string())?;
        if end > bytes.len() {
            return Err("wav chunk extends past end of file".to_string());
        }
        match id {
            b"fmt " => fmt = Some(parse_fmt_chunk(&bytes[start..end])?),
            b"data" => data = Some(&bytes[start..end]),
            _ => {}
        }
        off = end + (size & 1);
    }

    let fmt = fmt.ok_or_else(|| "wav is missing fmt chunk".to_string())?;
    let data = data.ok_or_else(|| "wav is missing data chunk".to_string())?;
    if fmt.channels == 0 {
        return Err("wav has zero channels".to_string());
    }
    if fmt.sample_rate == 0 {
        return Err("wav has zero sample rate".to_string());
    }

    let samples = match (fmt.audio_format, fmt.bits_per_sample) {
        (1, 16) => decode_pcm16(data, fmt.channels)?,
        (3, 32) => decode_float32(data, fmt.channels)?,
        (format, bits) => {
            return Err(format!(
                "unsupported wav format {format} with {bits} bits per sample"
            ))
        }
    };

    Ok(WavPcm {
        samples,
        sample_rate: fmt.sample_rate,
    })
}

#[derive(Debug, Clone, Copy)]
struct WavFmt {
    audio_format: u16,
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
}

fn parse_fmt_chunk(chunk: &[u8]) -> Result<WavFmt, String> {
    if chunk.len() < 16 {
        return Err("wav fmt chunk is too short".to_string());
    }
    Ok(WavFmt {
        audio_format: u16::from_le_bytes(chunk[0..2].try_into().unwrap()),
        channels: u16::from_le_bytes(chunk[2..4].try_into().unwrap()),
        sample_rate: u32::from_le_bytes(chunk[4..8].try_into().unwrap()),
        bits_per_sample: u16::from_le_bytes(chunk[14..16].try_into().unwrap()),
    })
}

fn decode_pcm16(data: &[u8], channels: u16) -> Result<Vec<i16>, String> {
    let channels = channels as usize;
    let frame_bytes = channels
        .checked_mul(2)
        .ok_or_else(|| "wav channel count overflows".to_string())?;
    if frame_bytes == 0 || data.len() % frame_bytes != 0 {
        return Err("wav PCM16 data is not frame-aligned".to_string());
    }
    let mut out = Vec::with_capacity(data.len() / frame_bytes);
    for frame in data.chunks_exact(frame_bytes) {
        let mut sum = 0i32;
        for ch in 0..channels {
            let i = ch * 2;
            sum += i16::from_le_bytes(frame[i..i + 2].try_into().unwrap()) as i32;
        }
        out.push((sum / channels as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    }
    Ok(out)
}

fn decode_float32(data: &[u8], channels: u16) -> Result<Vec<i16>, String> {
    let channels = channels as usize;
    let frame_bytes = channels
        .checked_mul(4)
        .ok_or_else(|| "wav channel count overflows".to_string())?;
    if frame_bytes == 0 || data.len() % frame_bytes != 0 {
        return Err("wav float32 data is not frame-aligned".to_string());
    }
    let mut out = Vec::with_capacity(data.len() / frame_bytes);
    for frame in data.chunks_exact(frame_bytes) {
        let mut sum = 0.0f32;
        for ch in 0..channels {
            let i = ch * 4;
            sum += f32::from_le_bytes(frame[i..i + 4].try_into().unwrap());
        }
        let mono = (sum / channels as f32).clamp(-1.0, 1.0);
        out.push((mono * 32767.0) as i16);
    }
    Ok(out)
}

/// Minimal linear-interpolation resampler (mono f32).
pub fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || to == 0 || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = input.get(idx).copied().unwrap_or(0.0);
        let b = input.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}

pub fn resample_i16_mono(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if from == to || from == 0 || to == 0 || samples.is_empty() {
        return samples.to_vec();
    }
    let f32_samples: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
    resample_linear(&f32_samples, from, to)
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn base64_decode_text(text: &str) -> Result<Vec<u8>, String> {
    let payload = text
        .trim()
        .strip_prefix("data:")
        .and_then(|s| s.split_once(',').map(|(_, b64)| b64))
        .unwrap_or_else(|| text.trim());
    let mut out = Vec::with_capacity(payload.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    let mut saw_padding = false;
    for b in payload.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if b == b'=' {
            saw_padding = true;
            continue;
        }
        if saw_padding {
            return Err("base64 data after padding".to_string());
        }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err(format!("invalid base64 byte 0x{b:02x}")),
        } as u32;
        buf = (buf << 6) | val;
        bits += 6;
        while bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
        if bits > 0 {
            buf &= (1 << bits) - 1;
        } else {
            buf = 0;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_pcm16_round_trips() {
        let samples = [-32768, -1000, 0, 1000, 32767];
        let wav = encode_wav_pcm16_mono(&samples, 16_000);
        let decoded = decode_wav_mono_i16(&wav).unwrap();
        assert_eq!(decoded.sample_rate, 16_000);
        assert_eq!(decoded.samples, samples);
    }

    #[test]
    fn wav_decode_downmixes_stereo_pcm16() {
        let mut wav = encode_wav_pcm16_mono(&[], 8_000);
        wav[22..24].copy_from_slice(&2u16.to_le_bytes()); // channels
        wav[28..32].copy_from_slice(&(8_000u32 * 4).to_le_bytes()); // byte rate
        wav[32..34].copy_from_slice(&4u16.to_le_bytes()); // block align
        let stereo = [1000i16, -1000, 3000, 1000];
        let data_len = stereo.len() * 2;
        wav.truncate(44);
        wav[4..8].copy_from_slice(&(36u32 + data_len as u32).to_le_bytes());
        wav[40..44].copy_from_slice(&(data_len as u32).to_le_bytes());
        for s in stereo {
            wav.extend_from_slice(&s.to_le_bytes());
        }

        let decoded = decode_wav_mono_i16(&wav).unwrap();
        assert_eq!(decoded.sample_rate, 8_000);
        assert_eq!(decoded.samples, vec![0, 2000]);
    }

    #[test]
    fn base64_handles_data_urls() {
        let bytes = b"hello wav";
        let b64 = base64_encode(bytes);
        assert_eq!(base64_decode_text(&b64).unwrap(), bytes);
        let data_url = format!("data:audio/wav;base64,{b64}");
        assert_eq!(base64_decode_text(&data_url).unwrap(), bytes);
    }

    #[test]
    fn resample_i16_changes_length() {
        let samples = vec![0i16; 16_000];
        let out = resample_i16_mono(&samples, 16_000, 8_000);
        assert_eq!(out.len(), 8_000);
    }
}
