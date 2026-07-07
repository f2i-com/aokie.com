//! Minimal SentencePiece detokenizer for Parakeet's `tokenizer.model`.
//!
//! Parakeet decodes RNN-T outputs to token ids; this module maps those
//! ids back to text. Encoding direction is not implemented — we never
//! feed text into the model.
//!
//! The `tokenizer.model` file is a SentencePiece `ModelProto` protobuf.
//! We parse only the `pieces` field (a repeated `(piece_str, score, type)`
//! triple) and ignore the rest. Same approach as
//! `runtimes/onnx_tts/tokenizer.rs`, scoped down to detokenisation only.

use std::path::Path;

const SP_SPACE: char = '\u{2581}'; // ▁ (SP word-boundary marker)

#[derive(Debug)]
pub struct Detokenizer {
    /// Index = SentencePiece id, value = piece string.
    pieces: Vec<String>,
}

impl Detokenizer {
    pub fn open(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let pieces = parse_pieces(&bytes)?;
        if pieces.is_empty() {
            return Err("tokenizer.model contained no pieces".into());
        }
        Ok(Self { pieces })
    }

    /// SentencePiece vocab size (pieces table length). Parakeet's
    /// blank token sits at index `vocab_size()` (one past the last
    /// real piece) per the model card; the caller checks that.
    pub fn vocab_size(&self) -> usize {
        self.pieces.len()
    }

    /// Convert a sequence of SP ids to a plain string.
    /// Concatenates pieces and replaces the `▁` space sentinel with
    /// real spaces. Strips the leading space SentencePiece prepends to
    /// the first word.
    pub fn decode(&self, ids: &[u32]) -> String {
        let mut out = String::new();
        for &id in ids {
            let idx = id as usize;
            if idx < self.pieces.len() {
                out.push_str(&self.pieces[idx]);
            }
        }
        let replaced: String = out
            .chars()
            .map(|c| if c == SP_SPACE { ' ' } else { c })
            .collect();
        replaced.trim_start().to_string()
    }
}

/* -----------------------------------------------------------------------
 * Minimal protobuf decoder for SentencePiece `ModelProto`.
 *
 *     message SentencePiece {
 *         optional string piece = 1;
 *         optional float  score = 2;  // unused here
 *         optional Type   type  = 3;  // unused here
 *     }
 *     message ModelProto {
 *         repeated SentencePiece pieces = 1;
 *         // ... other fields skipped ...
 *     }
 *
 * Wire format: each field is preceded by a varint tag whose low 3 bits
 * encode the wire type (0=varint, 1=64-bit, 2=length-delimited, 5=32-bit).
 * --------------------------------------------------------------------- */

fn parse_pieces(data: &[u8]) -> Result<Vec<String>, String> {
    let mut pieces = Vec::new();
    let mut cursor = 0usize;
    while cursor < data.len() {
        let (tag, wire) = read_tag(data, &mut cursor)?;
        if tag == 1 && wire == 2 {
            let len = read_varint(data, &mut cursor)? as usize;
            let end = cursor
                .checked_add(len)
                .ok_or_else(|| "pieces length overflow".to_string())?;
            if end > data.len() {
                return Err("pieces length exceeds buffer".into());
            }
            pieces.push(parse_piece_string(&data[cursor..end])?);
            cursor = end;
        } else {
            skip_field(data, &mut cursor, wire)?;
        }
    }
    Ok(pieces)
}

fn parse_piece_string(data: &[u8]) -> Result<String, String> {
    let mut text = String::new();
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
            (_, w) => skip_field(data, &mut cursor, w)?,
        }
    }
    Ok(text)
}

fn read_tag(data: &[u8], cursor: &mut usize) -> Result<(u32, u32), String> {
    let v = read_varint(data, cursor)?;
    Ok(((v >> 3) as u32, (v & 0x7) as u32))
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
