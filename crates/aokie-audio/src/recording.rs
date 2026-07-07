//! Per-call WAV writer for caller-side audio.
//!
//! R8/P0-3: previously stubbed out — `CALL_RECORDING_WRITER_AVAILABLE`
//! was hard-coded `false` so the read commands `get_call_recording_*`
//! always returned `None`, and the consent wizard told the operator
//! recording wasn't shipped yet. This module is the SCO-PCM writer
//! that backs that promise.
//!
//! ## Design
//!
//! Off by default. Operators have to opt in via the consent wizard
//! ("Call recording" toggle) or Settings → Privacy & retention. Two-
//! party-consent jurisdictions (NSW Australia, much of the US, every
//! EU member state, etc.) require caller notification before
//! recording — that's an operator-policy call we surface in the
//! toggle copy but cannot enforce mechanically.
//!
//! Format: PCM mono, 16-bit signed little-endian, native SCO sample
//! rate (16 kHz mSBC / 8 kHz CVSD). Mono-only on purpose — capturing
//! the bot side too would need clock-aligned mixing or a stereo split
//! (caller L / bot R), and the bot's responses are already perfectly
//! preserved in the transcripts. v1 records only what isn't already
//! retrievable — the caller's voice.
//!
//! ## Lifecycle (owned by the bluetooth_commands.rs audio drain loop)
//!
//!   * `CallRecorder::open(...)` is called once per call after the
//!     first `AudioFrame` arrives — that's when we know the SCO
//!     sample rate the controller actually negotiated.
//!   * `write_samples(&[i16])` for each subsequent frame. Cheap:
//!     a buffered write of `samples.len() * 2` bytes.
//!   * `finalize()` on `CallTerminated`. Seeks back to the WAV
//!     header and rewrites the four size fields with the count of
//!     samples actually written.
//!
//! Crash-safety: if the operator force-quits mid-call, the WAV
//! header's size fields stay at 0 (their placeholder value) and the
//! file is unplayable in most decoders. That's intentional — a
//! half-recorded call shouldn't masquerade as a complete one.
//! Operators can still hand the broken file to support; the data
//! chunk's bytes are valid PCM.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const BITS_PER_SAMPLE: u16 = 16;
const CHANNELS: u16 = 1;
/// Cap on how big a single recording can grow. 10 hours at 16 kHz
/// mono i16 = ~1.1 GB. Past this we stop writing rather than keep
/// growing — guards against a stuck call that never terminates
/// (controller drop, dongle replug missed by the runtime) burning
/// the disk.
const MAX_BYTES: u64 = 1_200_000_000;

pub struct CallRecorder {
    file: BufWriter<File>,
    path: PathBuf,
    sample_rate: u32,
    samples_written: u64,
    over_limit: bool,
}

impl CallRecorder {
    /// Open `<app_data>/recordings/<call_id>.wav` for streaming
    /// caller PCM. Creates the parent directory if needed. Writes a
    /// placeholder WAV header that `finalize` later fills in with
    /// the actual sample count.
    ///
    /// `sample_rate` must come from the live SCO link rate (8000
    /// CVSD / 16000 mSBC). Passing 0 is rejected — that would
    /// produce a header decoders refuse to play.
    pub fn open(call_id: &str, sample_rate: u16, app_data_dir: &Path) -> Result<Self, String> {
        if sample_rate == 0 {
            return Err("recording: sample_rate must be > 0".into());
        }
        if !is_valid_call_id(call_id) {
            return Err(format!(
                "recording: invalid call_id {:?} (alphanumeric / - / _ only, 1..=128 chars)",
                call_id
            ));
        }
        let recordings_dir = app_data_dir.join("recordings");
        std::fs::create_dir_all(&recordings_dir)
            .map_err(|e| format!("recording: create {:?}: {}", recordings_dir, e))?;
        let path = recordings_dir.join(format!("{}.wav", call_id));
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("recording: open {:?}: {}", path, e))?;
        let mut file = BufWriter::new(f);
        write_wav_header(&mut file, sample_rate as u32, 0)
            .map_err(|e| format!("recording: write placeholder header: {}", e))?;
        Ok(Self {
            file,
            path,
            sample_rate: sample_rate as u32,
            samples_written: 0,
            over_limit: false,
        })
    }

    /// Append `samples` to the data chunk. Caller is responsible for
    /// passing PCM at the same sample rate the recorder was opened
    /// with — a codec renegotiation mid-call would corrupt the
    /// timeline, so call sites must close + reopen if the rate flips.
    pub fn write_samples(&mut self, samples: &[i16]) -> Result<(), String> {
        if samples.is_empty() || self.over_limit {
            return Ok(());
        }
        // 10 h cap. Stop writing past the limit but keep the file
        // open so `finalize` still runs and produces a valid header
        // for the bytes already on disk.
        let after = self
            .samples_written
            .saturating_add(samples.len() as u64)
            .saturating_mul(2);
        if after > MAX_BYTES {
            self.over_limit = true;
            eprintln!(
                "[recording] {:?} hit {}-byte ceiling; stopping writes (call still in progress)",
                self.path, MAX_BYTES
            );
            return Ok(());
        }
        let mut buf = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        self.file
            .write_all(&buf)
            .map_err(|e| format!("recording: write {:?}: {}", self.path, e))?;
        self.samples_written += samples.len() as u64;
        Ok(())
    }

    /// Path the recording lives at. Useful for the call-log row's
    /// `recording_path` column once that ships, plus regression
    /// tests.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sample count actually written (regardless of the cap).
    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    /// Flush, seek back to the header, and rewrite the four size
    /// fields. Consumes `self`. After this returns Ok, the file is
    /// a valid mono PCM WAV that any decoder will play.
    pub fn finalize(mut self) -> Result<PathBuf, String> {
        self.file
            .flush()
            .map_err(|e| format!("recording: flush {:?}: {}", self.path, e))?;
        let mut inner = self
            .file
            .into_inner()
            .map_err(|e| format!("recording: unwrap BufWriter on {:?}: {}", self.path, e))?;
        inner
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("recording: seek {:?}: {}", self.path, e))?;
        write_wav_header(&mut inner, self.sample_rate, self.samples_written)
            .map_err(|e| format!("recording: rewrite header on {:?}: {}", self.path, e))?;
        inner
            .flush()
            .map_err(|e| format!("recording: final flush {:?}: {}", self.path, e))?;
        Ok(self.path)
    }
}

/// Emit the 44-byte canonical PCM mono WAV header into `w` for
/// `num_samples` 16-bit samples at `sample_rate` Hz. Used both at
/// open (with `num_samples = 0`) and at finalize (with the real
/// count). Splitting it into its own function means the open and
/// finalize paths can't drift on field layout.
fn write_wav_header<W: Write>(
    w: &mut W,
    sample_rate: u32,
    num_samples: u64,
) -> std::io::Result<()> {
    let byte_rate = sample_rate * (CHANNELS as u32) * (BITS_PER_SAMPLE as u32 / 8);
    let block_align = CHANNELS * (BITS_PER_SAMPLE / 8);
    // The data chunk's size is sample_count × bytes-per-sample,
    // saturated at u32 (WAV's max). The chunk-size field is
    // 36 + data_size.
    let data_size_u64 = num_samples.saturating_mul(2);
    let data_size = if data_size_u64 > u32::MAX as u64 {
        u32::MAX
    } else {
        data_size_u64 as u32
    };
    let chunk_size = 36u32.saturating_add(data_size);

    w.write_all(b"RIFF")?;
    w.write_all(&chunk_size.to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&CHANNELS.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&BITS_PER_SAMPLE.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_size.to_le_bytes())?;
    Ok(())
}

/// Mirror of `validate_call_id` in `bluetooth_commands.rs` (same
/// alphabet, same length cap). Kept as a module-local check so the
/// writer also rejects malicious / malformed ids on the off chance
/// the call-site contract drifts — the file path resolves under
/// `app_data/recordings/`, so a `..` / `/` / NUL injected via the
/// call_id would otherwise land outside the recordings dir.
fn is_valid_call_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aokie-recording-test-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Smallest possible round-trip: open, write, finalize, read
    /// back the file and verify the header math + sample bytes.
    #[test]
    fn round_trip_writes_canonical_wav() {
        let tmp = TestDir::new();
        let mut rec = CallRecorder::open("call-001", 16000, tmp.path()).unwrap();
        let samples: Vec<i16> = (0..100i16).collect();
        rec.write_samples(&samples).unwrap();
        let path = rec.finalize().unwrap();

        let bytes = fs::read(&path).unwrap();
        // 44-byte header + 100 × 2 bytes = 244.
        assert_eq!(bytes.len(), 244);
        // RIFF chunk header.
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 236);
        assert_eq!(&bytes[8..12], b"WAVE");
        // fmt chunk.
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 16000);
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 32000);
        assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        // data chunk.
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 200);
        // First sample (0) at offset 44, little-endian.
        assert_eq!(&bytes[44..46], &0i16.to_le_bytes());
        // Last sample (99) at offset 44 + 99*2 = 242.
        assert_eq!(&bytes[242..244], &99i16.to_le_bytes());
    }

    /// Multi-write paths must accumulate samples across calls and
    /// only commit the final size at finalize. Without
    /// `samples_written` accumulation across writes, the header
    /// would only reflect the last chunk.
    #[test]
    fn multiple_writes_accumulate() {
        let tmp = TestDir::new();
        let mut rec = CallRecorder::open("multi", 8000, tmp.path()).unwrap();
        rec.write_samples(&[1, 2, 3]).unwrap();
        rec.write_samples(&[4, 5]).unwrap();
        rec.write_samples(&[]).unwrap();
        rec.write_samples(&[6, 7, 8, 9]).unwrap();
        assert_eq!(rec.samples_written(), 9);
        let path = rec.finalize().unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 9 * 2);
        // data-size in header.
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 18);
    }

    /// An operator who closes a recording with no caller speech (call
    /// rejected before SCO came up, etc.) should still get a valid
    /// zero-length WAV — header reads 0-byte data chunk, players
    /// open it and play silence rather than rejecting it.
    #[test]
    fn finalize_with_zero_writes_emits_valid_empty_wav() {
        let tmp = TestDir::new();
        let rec = CallRecorder::open("empty", 16000, tmp.path()).unwrap();
        let path = rec.finalize().unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 44);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0);
    }

    /// CVSD calls run at 8 kHz; mSBC at 16 kHz. Header byte_rate +
    /// sample_rate must follow the link rate, not a hard-coded
    /// constant. Regression for "recording sounds chipmunky" if the
    /// rate ever silently defaults.
    #[test]
    fn cvsd_8khz_header_is_8khz() {
        let tmp = TestDir::new();
        let rec = CallRecorder::open("cvsd", 8000, tmp.path()).unwrap();
        let path = rec.finalize().unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 8000);
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 16000);
    }

    /// Path-traversal defence: any call_id outside the safe alphabet
    /// must be rejected before File::open. Without this, a renderer
    /// or future caller passing `"../foo"` could land the WAV outside
    /// the recordings dir. Same alphabet as
    /// `bluetooth_commands::validate_call_id`: `[A-Za-z0-9_-]`,
    /// 1..=128 chars, no path separators or dots.
    #[test]
    fn rejects_path_traversal_call_id() {
        let tmp = TestDir::new();
        assert!(CallRecorder::open("../escape", 16000, tmp.path()).is_err());
        assert!(CallRecorder::open("a/b", 16000, tmp.path()).is_err());
        assert!(CallRecorder::open("", 16000, tmp.path()).is_err());
        assert!(CallRecorder::open("a.b", 16000, tmp.path()).is_err());
        assert!(CallRecorder::open("ok-id_123", 16000, tmp.path()).is_ok());
    }

    /// 0 sample_rate would write a header decoders refuse to play
    /// (and hand a divide-by-zero risk to anyone computing duration
    /// from byte_rate). Reject at open time instead of producing
    /// the broken file.
    #[test]
    fn rejects_zero_sample_rate() {
        let tmp = TestDir::new();
        assert!(CallRecorder::open("zero-rate", 0, tmp.path()).is_err());
    }
}
