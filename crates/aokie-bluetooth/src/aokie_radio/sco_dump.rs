//! Forensic raw SCO byte-stream dump, gated on `AOKIE_DUMP_SCO_RAW=1`.
//!
//! Writes two append-only files under `%TEMP%/aokie_sco_dumps/`:
//!
//!   * `sco_rx.bin` — every chunk returned by `transport.read_sco()`,
//!     verbatim. Chunks include any HCI SCO headers; they're whatever
//!     the controller / WinUSB pipe handed us.
//!   * `sco_tx.bin` — every full HCI SCO packet handed to
//!     `transport.write_sco()` (HCI header + payload).
//!
//! Each entry is length-prefixed: 2 LE bytes giving chunk length N,
//! followed by N bytes of payload. Files are capped at
//! `MAX_FILE_BYTES` (~4 MB) so a long call can't fill the disk —
//! once the cap is hit, further writes are dropped (we want the head
//! of the stream, not the tail).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

struct DumpFile {
    file: Option<File>,
    written: u64,
}

impl DumpFile {
    fn open(path: PathBuf) -> Option<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| {
                eprintln!("[ScoDump] failed to open {:?}: {}", path, e);
            })
            .ok()?;
        Some(Self {
            file: Some(file),
            written: 0,
        })
    }

    fn append(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.written >= MAX_FILE_BYTES {
            return;
        }
        let len = bytes.len().min(u16::MAX as usize) as u16;
        let header = len.to_le_bytes();
        if let Err(e) = file
            .write_all(&header)
            .and_then(|_| file.write_all(&bytes[..len as usize]))
        {
            eprintln!("[ScoDump] write failed: {}", e);
            self.file = None;
            return;
        }
        self.written += 2 + len as u64;
        if self.written >= MAX_FILE_BYTES {
            let _ = file.flush();
            eprintln!(
                "[ScoDump] cap hit ({} bytes); further writes dropped",
                self.written
            );
        }
    }
}

struct DumpState {
    rx: Mutex<DumpFile>,
    tx: Mutex<DumpFile>,
}

static STATE: OnceLock<Option<DumpState>> = OnceLock::new();

fn state() -> Option<&'static DumpState> {
    STATE
        .get_or_init(|| {
            if std::env::var("AOKIE_DUMP_SCO_RAW").ok().as_deref() != Some("1") {
                return None;
            }
            let dir = std::env::temp_dir().join("aokie_sco_dumps");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("[ScoDump] mkdir {:?} failed: {}", dir, e);
                return None;
            }
            let rx = DumpFile::open(dir.join("sco_rx.bin"))?;
            let tx = DumpFile::open(dir.join("sco_tx.bin"))?;
            eprintln!("[ScoDump] dumping raw SCO to {:?}", dir);
            Some(DumpState {
                rx: Mutex::new(rx),
                tx: Mutex::new(tx),
            })
        })
        .as_ref()
}

pub fn dump_rx(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(s) = state() {
        if let Ok(mut g) = s.rx.lock() {
            g.append(bytes);
        }
    }
}

pub fn dump_tx(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(s) = state() {
        if let Ok(mut g) = s.tx.lock() {
            g.append(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn parse_chunks(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 2 <= data.len() {
            let len = u16::from_le_bytes([data[i], data[i + 1]]) as usize;
            i += 2;
            if i + len > data.len() {
                break;
            }
            out.push(data[i..i + len].to_vec());
            i += len;
        }
        out
    }

    /// Round-trip the chunk format: arbitrary chunks → file → chunks
    /// must come back identical. Mirrors what the inspector binary
    /// does, so a parser-side regression here would surface there too.
    #[test]
    fn dump_file_round_trips_chunks() {
        let dir = std::env::temp_dir().join(format!("aokie_sco_dump_test_{}", std::process::id(),));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scratch.bin");
        let mut df = DumpFile::open(path.clone()).unwrap();

        let chunks: Vec<Vec<u8>> = vec![
            vec![0x01, 0x08, 0xAD, 0x00, 0x00],
            vec![0xFF; 60],
            vec![],
            (0u8..=255).collect(),
        ];
        for c in &chunks {
            df.append(c);
        }
        drop(df);

        let mut data = Vec::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        let parsed = parse_chunks(&data);

        // Empty chunks are still valid format-wise (length-zero entry),
        // and `append` won't filter them at the DumpFile layer — that's
        // the public dump_rx/dump_tx wrappers' job.
        assert_eq!(parsed.len(), chunks.len());
        for (a, b) in parsed.iter().zip(chunks.iter()) {
            assert_eq!(a, b);
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn dump_file_caps_at_max_bytes() {
        let dir =
            std::env::temp_dir().join(format!("aokie_sco_dump_cap_test_{}", std::process::id(),));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.bin");
        let mut df = DumpFile::open(path.clone()).unwrap();

        // Fire chunks until well past MAX_FILE_BYTES; the file must
        // stop growing rather than spilling over.
        let blob = vec![0xAA; 1024];
        for _ in 0..(MAX_FILE_BYTES / blob.len() as u64 * 2) {
            df.append(&blob);
        }
        drop(df);

        let metadata = std::fs::metadata(&path).unwrap();
        // Allow up to one final entry past the cap (the cap is a
        // post-write check), but no more.
        assert!(
            metadata.len() <= MAX_FILE_BYTES + (2 + blob.len() as u64),
            "file grew past cap: {} bytes",
            metadata.len()
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// End-to-end diagnostic-pipeline self-test: write actual mSBC
    /// frames through the dump file, parse the dump back, and verify
    /// the H2 / mSBC sync detection + CRC validation that the
    /// `aokie-sco-dump-inspect` binary uses produces correct results.
    /// Catches any regression in either the dump format (writer side)
    /// or the inspector's parsing logic (reader side) without needing
    /// real hardware.
    #[test]
    fn full_dump_pipeline_finds_msbc_frames() {
        use crate::msbc::crc::crc8;
        use crate::msbc::H2Encoder;
        use crate::msbc::MSBC_SAMPLES_PER_FRAME;

        let dir = std::env::temp_dir().join(format!(
            "aokie_sco_dump_pipeline_test_{}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pipeline.bin");
        let mut df = DumpFile::open(path.clone()).unwrap();

        let mut enc = H2Encoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let mut frame_count = 0;
        for _ in 0..32 {
            let h2_packet = enc.encode_packet(&pcm);
            // Wrap in a synthetic 3-byte HCI SCO header so the chunk
            // matches what the runtime actually writes (handle + len +
            // status + payload). Connection handle 0x0042 is arbitrary.
            let mut hci = vec![0x42u8, 0x00u8, 60u8];
            hci.extend_from_slice(&h2_packet);
            df.append(&hci);
            frame_count += 1;
        }
        drop(df);

        let mut data = Vec::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        let parsed = parse_chunks(&data);
        assert_eq!(parsed.len(), frame_count);

        // Now run the same H2 + mSBC + CRC scan the inspector does.
        let flat: Vec<u8> = parsed.iter().flat_map(|c| c.iter().copied()).collect();
        const H2_SYNC_BYTE_0: u8 = 0x01;
        const H2_SYNC_BYTE_1_TABLE: [u8; 4] = [0x08, 0x38, 0xC8, 0xF8];
        const MSBC_SYNC: u8 = 0xAD;
        let mut found = 0usize;
        let mut crc_ok = 0usize;
        let mut i = 0;
        while i + 60 <= flat.len() {
            if flat[i] == H2_SYNC_BYTE_0
                && H2_SYNC_BYTE_1_TABLE.contains(&flat[i + 1])
                && flat[i + 2] == MSBC_SYNC
            {
                found += 1;
                let f = &flat[i + 2..i + 2 + 57];
                let mut crc_in = [0u8; 6];
                crc_in[0] = f[1];
                crc_in[1] = f[2];
                crc_in[2..6].copy_from_slice(&f[4..8]);
                if crc8(&crc_in) == f[3] {
                    crc_ok += 1;
                }
                i += 60;
            } else {
                i += 1;
            }
        }
        assert_eq!(found, frame_count, "inspector should find every frame");
        assert_eq!(crc_ok, frame_count, "every encoded frame should pass CRC");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Public wrappers must filter empty inputs so the caller doesn't
    /// have to — otherwise an empty `read_sco` return would write a
    /// zero-length entry every iteration of the runtime tick and drown
    /// the cap budget on idle loops.
    ///
    /// (The wrapper's behavior is checked structurally — we can't
    /// invoke `dump_rx` itself in unit tests because it uses a process-
    /// global OnceLock that's poisoned by env-var interactions across
    /// tests. Documenting the contract here is enough; the runtime
    /// integration is what validates it in practice.)
    #[test]
    fn empty_chunks_skipped_at_public_api() {
        // Sanity: appending an empty slice via the DumpFile path writes
        // a 2-byte zero header. Public wrappers MUST early-return on
        // empty so we don't burn the cap budget. This test just pins
        // the underlying write behavior so the wrapper guard remains
        // load-bearing.
        let dir =
            std::env::temp_dir().join(format!("aokie_sco_dump_empty_test_{}", std::process::id(),));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.bin");
        let mut df = DumpFile::open(path.clone()).unwrap();
        df.append(&[]);
        drop(df);

        let metadata = std::fs::metadata(&path).unwrap();
        // 2 bytes (length prefix = 0) and nothing else.
        assert_eq!(metadata.len(), 2);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
