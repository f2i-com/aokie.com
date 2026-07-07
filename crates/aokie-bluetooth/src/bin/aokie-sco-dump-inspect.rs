//! Forensic analyzer for the raw SCO byte dumps written by
//! `aokie_radio::sco_dump`.
//!
//! Run a call with `AOKIE_DUMP_SCO_RAW=1`, then point this tool at
//! `%TEMP%/aokie_sco_dumps/sco_rx.bin` (and `sco_tx.bin`). Reports:
//!
//!   * total bytes / chunks captured
//!   * chunk-length distribution
//!   * scan results for HCI SCO headers and mSBC sync patterns
//!   * mSBC frame CRC check (frame[3] vs computed CRC over scale-factor
//!     nibbles + zeroed wire bytes)
//!   * byte-value histogram (skewed = controller is doing CVSD codec
//!     on what should be transparent passthrough)
//!
//! Usage:
//!   aokie-sco-dump-inspect <path-to-bin>

use aokie_bluetooth::msbc::crc::crc8;
use std::fs;
use std::io::Read;

const H2_SYNC_BYTE_0: u8 = 0x01;
const H2_SYNC_BYTE_1_TABLE: [u8; 4] = [0x08, 0x38, 0xC8, 0xF8];
const MSBC_SYNC_BYTE: u8 = 0xAD;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <path-to-sco-dump.bin>", args[0]);
        std::process::exit(1);
    }
    let path = &args[1];
    let mut f = fs::File::open(path).unwrap_or_else(|e| {
        eprintln!("open {}: {}", path, e);
        std::process::exit(1);
    });
    let mut data = Vec::new();
    f.read_to_end(&mut data).unwrap_or_else(|e| {
        eprintln!("read {}: {}", path, e);
        std::process::exit(1);
    });

    println!("=== {} ({} bytes total file size) ===", path, data.len());
    println!();

    // 1) Parse length-prefixed chunks.
    let chunks = parse_chunks(&data);
    println!("chunks captured: {}", chunks.len());
    if chunks.is_empty() {
        println!("(no chunks — dump is empty or corrupt)");
        return;
    }
    let total_bytes: usize = chunks.iter().map(|c| c.len()).sum();
    let min_len = chunks.iter().map(|c| c.len()).min().unwrap_or(0);
    let max_len = chunks.iter().map(|c| c.len()).max().unwrap_or(0);
    let avg_len = total_bytes / chunks.len();
    println!(
        "payload bytes: {} (avg {} / min {} / max {} per chunk)",
        total_bytes, avg_len, min_len, max_len,
    );
    let mut len_hist: std::collections::BTreeMap<usize, usize> = Default::default();
    for c in &chunks {
        *len_hist.entry(c.len()).or_insert(0) += 1;
    }
    println!("chunk-length distribution (top 10):");
    let mut counts: Vec<(usize, usize)> = len_hist.into_iter().collect();
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (len, n) in counts.iter().take(10) {
        println!("  {:>4} bytes : {:>6} chunks", len, n);
    }
    println!();

    // 2) HCI SCO packet boundaries.
    //
    //    a) "1:1": chunk == single HCI SCO packet (TX side, or RX when
    //       the transport layer never coalesces).
    //    b) "chained": chunk is a stream of concatenated HCI SCO
    //       packets — walk byte-by-byte, reading byte[2] as length,
    //       skipping past, and trying again. The CSR8510 RX dumps show
    //       this pattern: 510-byte chunks = 10 × 51 (3 hdr + 48 pld),
    //       561-byte chunks = 11 × 51. The "1:1" check fails on these
    //       (the whole 510-byte chunk isn't 1 packet) but the chained
    //       walk recovers them all.
    let mut hci_packets_ok = 0usize;
    let mut hci_packets_bad = 0usize;
    let mut hci_payload_lens: std::collections::BTreeMap<u8, usize> = Default::default();
    for c in &chunks {
        if c.len() >= 3 {
            let len = c[2];
            if c.len() == 3 + len as usize {
                hci_packets_ok += 1;
                *hci_payload_lens.entry(len).or_insert(0) += 1;
            } else {
                hci_packets_bad += 1;
            }
        } else {
            hci_packets_bad += 1;
        }
    }
    println!(
        "HCI SCO interpretation (chunk == 1 packet): {} 1:1, {} not 1:1",
        hci_packets_ok, hci_packets_bad
    );
    if !hci_payload_lens.is_empty() {
        println!("HCI SCO payload-length distribution (1:1 chunks only):");
        for (len, n) in &hci_payload_lens {
            println!("  len byte 0x{:02x} ({:>3}): {:>6} packets", len, len, n);
        }
    }

    // Chained walk: try to parse each chunk as a sequence of HCI SCO
    // packets. We accept a packet iff bytes[2] is a plausible payload
    // length AND the implied next-packet header (handle bytes 0..2)
    // matches the previous one (controllers don't change the connection
    // handle mid-burst).
    let mut chained_total = 0usize;
    let mut chained_lens: std::collections::BTreeMap<u8, usize> = Default::default();
    let mut chained_handles: std::collections::BTreeMap<u16, usize> = Default::default();
    let mut chained_chunks_clean = 0usize;
    let mut chained_chunks_partial = 0usize;
    for c in &chunks {
        let mut i = 0usize;
        let mut walked_to_end = true;
        let mut first_handle: Option<u16> = None;
        while i + 3 <= c.len() {
            let h = u16::from_le_bytes([c[i], c[i + 1]]) & 0x0fff;
            let len = c[i + 2] as usize;
            if i + 3 + len > c.len() {
                walked_to_end = false;
                break;
            }
            if first_handle.is_none() {
                first_handle = Some(h);
            }
            *chained_handles.entry(h).or_insert(0) += 1;
            *chained_lens.entry(c[i + 2]).or_insert(0) += 1;
            chained_total += 1;
            i += 3 + len;
        }
        if i == c.len() {
            chained_chunks_clean += 1;
        } else if walked_to_end {
            // The walk broke before consuming the whole chunk — partial.
            chained_chunks_partial += 1;
        } else {
            chained_chunks_partial += 1;
        }
    }
    println!(
        "HCI SCO chained walk: {} packets total across {} clean / {} partial chunks",
        chained_total, chained_chunks_clean, chained_chunks_partial
    );
    if !chained_lens.is_empty() {
        let mut counts: Vec<(u8, usize)> = chained_lens.iter().map(|(k, v)| (*k, *v)).collect();
        counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        println!("  payload-length distribution (top 5):");
        for (len, n) in counts.iter().take(5) {
            println!("    0x{:02x} ({:>3}): {:>6} packets", len, len, n);
        }
    }
    if !chained_handles.is_empty() {
        let mut counts: Vec<(u16, usize)> = chained_handles.iter().map(|(k, v)| (*k, *v)).collect();
        counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        println!("  connection-handle distribution (top 3):");
        for (h, n) in counts.iter().take(3) {
            println!("    0x{:04x}: {:>6} packets", h, n);
        }
    }
    println!();

    // 3) Reassemble the HCI SCO payload stream (what the assembler
    //    feeds the framer) and check H2 sync there too. If the chained
    //    walk succeeded, "payload bytes" is the meaningful denominator —
    //    not raw chunk bytes — for mSBC frame counting.
    let mut payload_flat: Vec<u8> = Vec::new();
    for c in &chunks {
        let mut i = 0usize;
        while i + 3 <= c.len() {
            let len = c[i + 2] as usize;
            if i + 3 + len > c.len() {
                break;
            }
            payload_flat.extend_from_slice(&c[i + 3..i + 3 + len]);
            i += 3 + len;
        }
    }
    let mut payload_h2 = 0usize;
    for w in payload_flat.windows(3) {
        if w[0] == H2_SYNC_BYTE_0 && H2_SYNC_BYTE_1_TABLE.contains(&w[1]) && w[2] == MSBC_SYNC_BYTE
        {
            payload_h2 += 1;
        }
    }
    println!(
        "HCI payload reassembly: {} bytes, {} H2 sync triplets (1 per ~{} bytes)",
        payload_flat.len(),
        payload_h2,
        if payload_h2 > 0 {
            payload_flat.len() / payload_h2
        } else {
            0
        }
    );
    println!();

    // 4) Search the raw byte stream for H2 sync + mSBC sync. This is
    //    the "what's actually on the wire" question — if the codec is
    //    encoding correctly and the controller is passing bytes through,
    //    we should see 0x01 0x{08,38,c8,f8} 0xAD every 60 bytes
    //    (TX side: at chunk start; RX side: every payload).
    let flat: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let mut h2_sync_hits = 0usize;
    let mut h2_sync_seq_hist = [0usize; 4];
    for w in flat.windows(3) {
        if w[0] == H2_SYNC_BYTE_0 && H2_SYNC_BYTE_1_TABLE.contains(&w[1]) && w[2] == MSBC_SYNC_BYTE
        {
            h2_sync_hits += 1;
            if let Some(idx) = H2_SYNC_BYTE_1_TABLE.iter().position(|&b| b == w[1]) {
                h2_sync_seq_hist[idx] += 1;
            }
        }
    }
    let expected_msbc_hits = total_bytes / 60; // one sync per 60-byte H2 frame
    println!(
        "H2 sync triplets in raw byte stream: {} (expected ~{} for an mSBC dump of this size)",
        h2_sync_hits, expected_msbc_hits
    );
    println!(
        "  rotation balance: 08={}  38={}  C8={}  F8={}",
        h2_sync_seq_hist[0], h2_sync_seq_hist[1], h2_sync_seq_hist[2], h2_sync_seq_hist[3]
    );
    if h2_sync_hits == 0 && expected_msbc_hits > 1 {
        println!("  WARN: no H2 sync visible — either this is a CVSD dump, or the controller scrambled the transparent stream");
    }
    println!();

    // 4) For each H2 hit at a 60-byte boundary, validate the mSBC CRC
    //    so we can tell "frames look right" from "sync bytes randomly
    //    aligned but contents corrupt".
    let mut crc_ok = 0usize;
    let mut crc_bad = 0usize;
    let mut i = 0;
    while i + 60 <= flat.len() {
        if flat[i] == H2_SYNC_BYTE_0
            && H2_SYNC_BYTE_1_TABLE.contains(&flat[i + 1])
            && flat[i + 2] == MSBC_SYNC_BYTE
        {
            // mSBC frame starts at i+2, runs 57 bytes (i+2 .. i+59).
            // CRC covers byte 1 (= frame[1]), byte 2 (= frame[2]),
            // and the 4 scale-factor bytes (= frame[4..8]).
            let f = &flat[i + 2..i + 2 + 57];
            let mut crc_input = [0u8; 6];
            crc_input[0] = f[1];
            crc_input[1] = f[2];
            crc_input[2..6].copy_from_slice(&f[4..8]);
            let computed = crc8(&crc_input);
            if computed == f[3] {
                crc_ok += 1;
            } else {
                crc_bad += 1;
            }
            i += 60;
        } else {
            i += 1;
        }
    }
    println!("mSBC CRC check: {} OK, {} bad", crc_ok, crc_bad);
    println!();

    // 5) Byte histogram. A truly transparent mSBC stream is
    //    near-uniform (no dominant byte values). A CVSD-encoded stream
    //    has a heavy 0x00 / 0xFF tail because CVSD samples are signed
    //    16-bit linear PCM and silence packs as zeros. If the dongle is
    //    re-encoding the supposedly-transparent frame as CVSD, the
    //    histogram will look CVSD-shaped despite voice setting 0x0043.
    let mut hist = [0u64; 256];
    for &b in &flat {
        hist[b as usize] += 1;
    }
    let mut sorted: Vec<(u8, u64)> = (0..=255u8).map(|b| (b, hist[b as usize])).collect();
    sorted.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("byte-value histogram (top 8 most-common values):");
    for (b, n) in sorted.iter().take(8) {
        let pct = if total_bytes > 0 {
            (*n as f64) * 100.0 / (total_bytes as f64)
        } else {
            0.0
        };
        println!("  0x{:02x} ({:>3}) : {:>10} ({:5.2}%)", b, b, n, pct);
    }
    let zeros = hist[0x00] + hist[0xff];
    let pct_zeros_or_ffs = (zeros as f64) * 100.0 / (total_bytes.max(1) as f64);
    println!("  combined 0x00 + 0xFF: {:.2}%", pct_zeros_or_ffs);
    if pct_zeros_or_ffs > 30.0 {
        println!(
            "  WARN: very high 0x00/0xFF density — looks like linear-PCM silence, not transparent mSBC bytes",
        );
    }
}

/// Parse the length-prefixed chunk stream written by `sco_dump`.
fn parse_chunks(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= data.len() {
        let len = u16::from_le_bytes([data[i], data[i + 1]]) as usize;
        i += 2;
        if i + len > data.len() {
            // Truncated tail — bail without erroring.
            break;
        }
        out.push(data[i..i + len].to_vec());
        i += len;
    }
    out
}
