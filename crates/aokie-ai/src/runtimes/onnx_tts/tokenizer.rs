//! Pure-Rust SentencePiece Unigram tokenizer.
//!
//! The `tokenizer.model` file shipped with pocket-tts is a SentencePiece
//! `ModelProto` — a protobuf message containing a `pieces` table of
//! `(piece_str, log_prob, type)` triples. We read just the `pieces` field
//! and run a standard Viterbi forward/backward trace to pick the most
//! likely segmentation of the input into known pieces. All of this in ~200
//! lines so we can stay off the C++ `sentencepiece-sys` crate, which trips
//! a UCRT debug assertion on Windows in dev builds.
//!
//! Scope: Unigram model only (which is what pocket-tts uses). BPE
//! segmentation would need a different algorithm and isn't required here.

use std::collections::HashMap;
use std::path::Path;

/// SentencePiece piece types from the proto schema. We only need to know
/// which piece id is the `UNKNOWN` fallback.
#[allow(dead_code)]
const PIECE_TYPE_NORMAL: i32 = 1;
const PIECE_TYPE_UNKNOWN: i32 = 2;
#[allow(dead_code)]
const PIECE_TYPE_CONTROL: i32 = 3;
#[allow(dead_code)]
const PIECE_TYPE_USER_DEFINED: i32 = 4;
#[allow(dead_code)]
const PIECE_TYPE_UNUSED: i32 = 5;
#[allow(dead_code)]
const PIECE_TYPE_BYTE: i32 = 6;

/// SentencePiece's word-boundary marker. Every real word in a pre-tokenized
/// SentencePiece string starts with this, standing in for a leading space.
/// Renders as U+2581 "LOWER ONE EIGHTH BLOCK".
const SP_SPACE: &str = "\u{2581}";

#[derive(Debug, Clone)]
struct Piece {
    text: String,
    score: f32,
    kind: i32,
}

/// Unigram SentencePiece tokenizer.
pub struct Tokenizer {
    pieces: Vec<Piece>,
    lookup: HashMap<String, u32>,
    unk_id: u32,
    max_piece_bytes: usize,
    /// Log-prob penalty to apply to UNK fallbacks during Viterbi. The
    /// piece's stored score is often ~0, which lets UNK paths dominate
    /// multi-piece matches. We use (min real-piece score − 10) so UNK is
    /// always the most expensive transition.
    unk_penalty: f32,
}

impl Tokenizer {
    pub fn open(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let pieces = parse_sentencepiece_model(&bytes)?;
        if pieces.is_empty() {
            return Err("tokenizer.model contained no pieces".into());
        }
        let mut lookup = HashMap::with_capacity(pieces.len());
        let mut unk_id: Option<u32> = None;
        let mut max_piece_bytes = 0usize;
        for (i, p) in pieces.iter().enumerate() {
            lookup.insert(p.text.clone(), i as u32);
            if p.kind == PIECE_TYPE_UNKNOWN && unk_id.is_none() {
                unk_id = Some(i as u32);
            }
            if p.text.len() > max_piece_bytes {
                max_piece_bytes = p.text.len();
            }
        }
        let unk_id = unk_id.ok_or_else(|| "no UNKNOWN piece in tokenizer.model".to_string())?;

        // Find the minimum score among the real pieces (excluding UNK and
        // other control tokens) so we can make UNK a strictly-worse
        // transition during Viterbi.
        let min_real_score = pieces
            .iter()
            .filter(|p| p.kind == PIECE_TYPE_NORMAL || p.kind == PIECE_TYPE_USER_DEFINED)
            .map(|p| p.score)
            .fold(f32::INFINITY, f32::min);
        let unk_penalty = if min_real_score.is_finite() {
            min_real_score - 10.0
        } else {
            -20.0
        };

        // Sanity logs: vocab size, max piece bytes, and a few sample pieces
        // around common IDs so we can spot malformed/truncated parsing.
        eprintln!(
            "[pocket_tts_onnx] tokenizer: {} pieces, max_piece_bytes={}, unk_id={} (piece score {:.3}, viterbi penalty {:.3})",
            pieces.len(),
            max_piece_bytes,
            unk_id,
            pieces[unk_id as usize].score,
            unk_penalty,
        );
        // Print piece 260 (seen in our diagnostic output) and a few common
        // English subwords if we can find them.
        if let Some(p260) = pieces.get(260) {
            eprintln!(
                "[pocket_tts_onnx]   piece[260] = {:?} (score {:.3}, kind {})",
                p260.text, p260.score, p260.kind
            );
        }
        for needle in ["How", "▁How", "H", "▁H", "Hello", "▁", "hello"] {
            match lookup.get(needle) {
                Some(id) => eprintln!(
                    "[pocket_tts_onnx]   lookup({:?}) = id {} score {:.3}",
                    needle, id, pieces[*id as usize].score
                ),
                None => eprintln!("[pocket_tts_onnx]   lookup({:?}) = MISSING", needle),
            }
        }

        Ok(Self {
            pieces,
            lookup,
            unk_id,
            max_piece_bytes,
            unk_penalty,
        })
    }

    /// Encode `text` into token ids. Applies SentencePiece's standard
    /// preprocessing: replace every space with the `▁` sentinel, add a
    /// leading `▁`, then Viterbi-segment into known pieces.
    pub fn encode(&self, text: &str) -> Result<Vec<u32>, String> {
        // Normalize spaces → `▁`. This mirrors SentencePiece's `pre_tokenize`
        // with the Metaspace pre-tokenizer.
        let with_marker = format!("{}{}", SP_SPACE, text.replace(' ', SP_SPACE));
        let bytes = with_marker.as_bytes();
        let n = bytes.len();
        if n == 0 {
            return Ok(Vec::new());
        }

        // Viterbi forward pass. `best[i]` holds the best log-prob of
        // reaching byte position i, along with the piece that got us there
        // and the start position of that piece.
        let neg_inf = f32::NEG_INFINITY;
        let mut best_score: Vec<f32> = vec![neg_inf; n + 1];
        let mut best_prev: Vec<usize> = vec![0; n + 1];
        let mut best_piece: Vec<u32> = vec![self.unk_id; n + 1];
        best_score[0] = 0.0;

        for i in 0..n {
            if best_score[i] == neg_inf {
                continue;
            }
            let max_end = (i + self.max_piece_bytes).min(n);
            let mut found_any = false;
            let mut j = i + 1;
            while j <= max_end {
                // We can only consider valid UTF-8 slice boundaries.
                if !is_char_boundary(bytes, j) {
                    j += 1;
                    continue;
                }
                // SAFETY: bounds + boundaries checked above.
                let piece_str =
                    std::str::from_utf8(&bytes[i..j]).map_err(|e| format!("utf8 slice: {e}"))?;
                if let Some(&pid) = self.lookup.get(piece_str) {
                    let score = best_score[i] + self.pieces[pid as usize].score;
                    if score > best_score[j] {
                        best_score[j] = score;
                        best_prev[j] = i;
                        best_piece[j] = pid;
                        found_any = true;
                    }
                }
                j += 1;
            }
            // Fallback: advance by one code point, scored with the UNK
            // penalty (stricter than the piece's stored score, which in
            // this model is ~0 and would let UNK paths dominate). This
            // matches SentencePiece's behaviour where UNK is the costliest
            // lattice transition — so it's only picked when no real piece
            // can span the gap. We ALWAYS consider the UNK transition,
            // regardless of whether other matches started at `i`, so a
            // multi-byte char with no piece match doesn't strand Viterbi.
            let step = char_width_at(bytes, i);
            let next = i + step;
            if next <= n {
                let score = best_score[i] + self.unk_penalty;
                if score > best_score[next] {
                    best_score[next] = score;
                    best_prev[next] = i;
                    best_piece[next] = self.unk_id;
                }
            }
            let _ = found_any; // kept for readability; no longer gates UNK
        }

        if best_score[n] == neg_inf {
            return Err(format!("tokenizer could not segment input (len={n})"));
        }

        // Backtrace.
        let mut ids: Vec<u32> = Vec::new();
        let mut pos = n;
        while pos > 0 {
            ids.push(best_piece[pos]);
            pos = best_prev[pos];
        }
        ids.reverse();
        Ok(ids)
    }
}

/// Is `i` a UTF-8 char boundary inside `bytes`? Includes both ends.
fn is_char_boundary(bytes: &[u8], i: usize) -> bool {
    if i == 0 || i == bytes.len() {
        return true;
    }
    // Non-continuation bytes start with bits != 10xx_xxxx.
    (bytes[i] & 0xC0) != 0x80
}

/// Byte width of the UTF-8 char starting at `i`.
fn char_width_at(bytes: &[u8], i: usize) -> usize {
    if i >= bytes.len() {
        return 1;
    }
    match bytes[i] {
        b if b < 0x80 => 1,
        b if (b & 0xE0) == 0xC0 => 2,
        b if (b & 0xF0) == 0xE0 => 3,
        b if (b & 0xF8) == 0xF0 => 4,
        _ => 1, // malformed — step by a byte and let UNK handle it
    }
}

/* -----------------------------------------------------------------------
 * Minimal protobuf decoder for SentencePiece `ModelProto`.
 *
 * The `ModelProto` schema we care about:
 *     message SentencePiece {
 *         optional string piece = 1;
 *         optional float  score = 2;
 *         optional Type   type  = 3 [default = NORMAL];
 *     }
 *     message ModelProto {
 *         repeated SentencePiece pieces = 1;
 *         // ...other fields we skip...
 *     }
 *
 * Wire format primer: every field is preceded by a varint-encoded tag
 * whose low 3 bits encode the wire type:
 *   0 = varint, 1 = 64-bit fixed, 2 = length-delimited, 5 = 32-bit fixed.
 * We care about wire types 0, 2, and 5 here.
 * --------------------------------------------------------------------- */

fn parse_sentencepiece_model(data: &[u8]) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut cursor = 0usize;
    while cursor < data.len() {
        let (tag, wire) = read_tag(data, &mut cursor)?;
        if tag == 1 && wire == 2 {
            // pieces (repeated SentencePiece — length-delimited message)
            let len = read_varint(data, &mut cursor)? as usize;
            let end = cursor
                .checked_add(len)
                .ok_or_else(|| "pieces length overflow".to_string())?;
            if end > data.len() {
                return Err("pieces length exceeds buffer".into());
            }
            pieces.push(parse_piece(&data[cursor..end])?);
            cursor = end;
        } else {
            skip_field(data, &mut cursor, wire)?;
        }
    }
    Ok(pieces)
}

fn parse_piece(data: &[u8]) -> Result<Piece, String> {
    let mut text = String::new();
    let mut score: f32 = 0.0;
    let mut kind: i32 = PIECE_TYPE_NORMAL;
    let mut cursor = 0usize;
    while cursor < data.len() {
        let (tag, wire) = read_tag(data, &mut cursor)?;
        match (tag, wire) {
            (1, 2) => {
                let len = read_varint(data, &mut cursor)? as usize;
                let end = cursor + len;
                if end > data.len() {
                    return Err("piece.piece overflow".into());
                }
                text = String::from_utf8(data[cursor..end].to_vec())
                    .map_err(|e| format!("piece utf8: {e}"))?;
                cursor = end;
            }
            (2, 5) => {
                // float32, little-endian 4 bytes
                if cursor + 4 > data.len() {
                    return Err("piece.score overflow".into());
                }
                let bytes = [
                    data[cursor],
                    data[cursor + 1],
                    data[cursor + 2],
                    data[cursor + 3],
                ];
                score = f32::from_le_bytes(bytes);
                cursor += 4;
            }
            (3, 0) => {
                kind = read_varint(data, &mut cursor)? as i32;
            }
            (_, w) => skip_field(data, &mut cursor, w)?,
        }
    }
    Ok(Piece { text, score, kind })
}

fn read_tag(data: &[u8], cursor: &mut usize) -> Result<(u32, u32), String> {
    let v = read_varint(data, cursor)?;
    let tag = (v >> 3) as u32;
    let wire = (v & 0x7) as u32;
    Ok((tag, wire))
}

fn read_varint(data: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        if *cursor >= data.len() {
            return Err("varint truncated".into());
        }
        let b = data[*cursor];
        *cursor += 1;
        result |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 63 {
            return Err("varint too long".into());
        }
    }
}

fn skip_field(data: &[u8], cursor: &mut usize, wire: u32) -> Result<(), String> {
    match wire {
        0 => {
            read_varint(data, cursor)?;
        }
        1 => {
            if *cursor + 8 > data.len() {
                return Err("i64 overflow".into());
            }
            *cursor += 8;
        }
        2 => {
            let len = read_varint(data, cursor)? as usize;
            if *cursor + len > data.len() {
                return Err("len-delim overflow".into());
            }
            *cursor += len;
        }
        5 => {
            if *cursor + 4 > data.len() {
                return Err("i32 overflow".into());
            }
            *cursor += 4;
        }
        other => return Err(format!("unknown wire type {other}")),
    }
    Ok(())
}
