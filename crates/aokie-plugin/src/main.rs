//! aokie-plugin binary: the NDJSON JSON-RPC 2.0 loop over stdio.
//!
//! Desktop spawns this with cwd = plugin dir and the env described in
//! DESKTOP_PLUGIN_SDK.md §2 (`FORMLOGIC_PLUGIN_DATA_DIR`,
//! `FORMLOGIC_DEV_MODE`, …). Stdout carries protocol lines ONLY;
//! anything else goes to stderr (Desktop captures it to the plugin
//! log ring buffer).

use std::io::{self, BufRead, Read};
use std::path::PathBuf;

use aokie_plugin::connector::Plugin;
use aokie_plugin::event_bridge::{Sink, StdoutSink};
use aokie_plugin::rpc;

/// Outcome of reading one capped protocol line.
enum LineRead {
    Eof,
    Line,
    TooLong,
}

/// Read one `\n`-terminated line into `buf`, enforcing the 1 MiB cap
/// without buffering an unbounded oversized line: once the cap is
/// crossed the remainder of the line is drained in bounded chunks.
fn read_line_capped(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<LineRead> {
    buf.clear();
    let n = (&mut *reader)
        .take((rpc::MAX_LINE_BYTES + 2) as u64)
        .read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    let newline_seen = buf.last() == Some(&b'\n');
    if newline_seen {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    if buf.len() > rpc::MAX_LINE_BYTES {
        if !newline_seen {
            // Cap hit mid-line: drain to the newline in bounded chunks.
            let mut scratch = Vec::with_capacity(64 * 1024);
            loop {
                scratch.clear();
                let m = (&mut *reader)
                    .take(64 * 1024)
                    .read_until(b'\n', &mut scratch)?;
                if m == 0 || scratch.last() == Some(&b'\n') {
                    break;
                }
            }
        }
        return Ok(LineRead::TooLong);
    }
    // A final line without a trailing newline (EOF) is still a line.
    Ok(LineRead::Line)
}

fn main() {
    let dev_mode = std::env::var("FORMLOGIC_DEV_MODE")
        .map(|v| v == "1")
        .unwrap_or(false);
    let data_dir = std::env::var_os("FORMLOGIC_PLUGIN_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("aokie-plugin"));

    let mut plugin = match Plugin::new(dev_mode, data_dir) {
        Ok(p) => p,
        Err(e) => {
            // Handshake will never succeed without storage; exit so
            // Desktop marks the plugin crashed with a visible reason.
            eprintln!("[aokie-plugin] fatal: {e}");
            std::process::exit(1);
        }
    };

    let stdin = io::stdin();
    let mut reader = io::BufReader::new(stdin.lock());
    let mut sink = StdoutSink::new();
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);

    loop {
        let read = match read_line_capped(&mut reader, &mut buf) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[aokie-plugin] stdin read error: {e}");
                break;
            }
        };
        match read {
            LineRead::Eof => break, // host closed stdin → shut down
            LineRead::TooLong => {
                let line = rpc::error_line(
                    None,
                    rpc::INVALID_REQUEST,
                    &format!("line exceeds {} bytes", rpc::MAX_LINE_BYTES),
                    None,
                );
                if sink.send_line(&line).is_err() {
                    break;
                }
            }
            LineRead::Line => {
                let text = match std::str::from_utf8(&buf) {
                    Ok(t) => t.trim(),
                    Err(_) => {
                        let line =
                            rpc::error_line(None, rpc::PARSE_ERROR, "line is not UTF-8", None);
                        if sink.send_line(&line).is_err() {
                            break;
                        }
                        continue;
                    }
                };
                if text.is_empty() {
                    continue;
                }
                // Guide P1-14 (bidirectional stdio): RESPONSES to requests
                // WE made (flow.run lookups) arrive on the same stdin —
                // id + result/error, no method. Route them to the waiting
                // caller; everything else dispatches as before.
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                    if plugin.host_rpc.try_route_response(&v) {
                        continue;
                    }
                }
                let response = match rpc::parse_line(text) {
                    Ok(msg) => plugin.handle_rpc(msg, &mut sink),
                    Err(e) => Some(rpc::error_line(e.id.as_ref(), e.code, &e.message, None)),
                };
                if let Some(line) = response {
                    if sink.send_line(&line).is_err() {
                        break; // stdout gone → host died; exit cleanly
                    }
                }
                if plugin.shutdown_requested {
                    break; // plugin.shutdown was acknowledged above
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_lines_and_eof() {
        let mut reader = io::BufReader::new(&b"hello\nworld"[..]);
        let mut buf = Vec::new();
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf).unwrap(),
            LineRead::Line
        ));
        assert_eq!(buf, b"hello");
        // Final line without newline still counts.
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf).unwrap(),
            LineRead::Line
        ));
        assert_eq!(buf, b"world");
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf).unwrap(),
            LineRead::Eof
        ));
    }

    #[test]
    fn strips_crlf() {
        let mut reader = io::BufReader::new(&b"ping\r\n"[..]);
        let mut buf = Vec::new();
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf).unwrap(),
            LineRead::Line
        ));
        assert_eq!(buf, b"ping");
    }

    #[test]
    fn oversized_line_is_rejected_and_drained() {
        let mut input = vec![b'x'; rpc::MAX_LINE_BYTES + 4096];
        input.push(b'\n');
        input.extend_from_slice(b"next\n");
        let mut reader = io::BufReader::new(&input[..]);
        let mut buf = Vec::new();
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf).unwrap(),
            LineRead::TooLong
        ));
        // The following line is still readable — the oversized one
        // was fully consumed.
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf).unwrap(),
            LineRead::Line
        ));
        assert_eq!(buf, b"next");
    }

    #[test]
    fn line_exactly_at_cap_passes() {
        let mut input = vec![b'y'; rpc::MAX_LINE_BYTES];
        input.push(b'\n');
        let mut reader = io::BufReader::new(&input[..]);
        let mut buf = Vec::new();
        assert!(matches!(
            read_line_capped(&mut reader, &mut buf).unwrap(),
            LineRead::Line
        ));
        assert_eq!(buf.len(), rpc::MAX_LINE_BYTES);
    }
}
