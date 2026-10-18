//! aokie-plugin binary: the NDJSON JSON-RPC 2.0 loop over stdio.
//!
//! Desktop spawns this with cwd = plugin dir and the env described in
//! DESKTOP_PLUGIN_SDK.md §2 (`FORMLOGIC_PLUGIN_DATA_DIR`,
//! `FORMLOGIC_DEV_MODE`, …). Stdout carries protocol lines ONLY;
//! anything else goes to stderr (Desktop captures it to the plugin
//! log ring buffer).

use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use aokie_plugin::connector::{Plugin, HOST_GRACE_RADIO_WAIT, HOST_GRACE_THREAD_WAIT};
use aokie_plugin::event_bridge::{Sink, StdoutSink};
use aokie_plugin::host_rpc::HostRpc;
use aokie_plugin::rpc;

/// After SIGTERM, OAIY Desktop sends SIGKILL 2 s later: the radio is given
/// this long to confirm its stop, and its thread this long more to close the
/// dongle.
const SIGNAL_RADIO_WAIT: std::time::Duration = std::time::Duration::from_millis(1200);
const SIGNAL_THREAD_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// What the main loop is handed, by the stdin reader and the signal watcher.
enum Input {
    /// One protocol line (its `\n` and any `\r` cut off).
    Line(Vec<u8>),
    /// A line over the cap (read and dropped).
    TooLong,
    /// stdin ended, or could not be read (with why): the host is done or gone.
    Closed(Option<String>),
    /// The process was told to stop (SIGTERM, Ctrl-C).
    Signal(&'static str),
}

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

/// Read stdin on a thread of its own and hand the main loop each line, then
/// `Closed` at its end. A line that answers a request the plugin made of the
/// host (guide P1-14: a mid-call `flow.run` lookup; id + result/error, no
/// method) goes to whoever waits for it from here, so a radio thread waiting
/// on the host is not held behind a slow command on the main loop.
fn spawn_stdin_reader(tx: mpsc::Sender<Input>, host_rpc: Arc<HostRpc>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("aokie-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut reader = io::BufReader::new(stdin.lock());
            let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
            loop {
                let input = match read_line_capped(&mut reader, &mut buf) {
                    Ok(LineRead::Eof) => Input::Closed(None),
                    Ok(LineRead::TooLong) => Input::TooLong,
                    Ok(LineRead::Line) => {
                        if is_host_response(&buf, &host_rpc) {
                            continue;
                        }
                        Input::Line(buf.clone())
                    }
                    Err(e) => Input::Closed(Some(e.to_string())),
                };
                let closed = matches!(input, Input::Closed(_));
                if tx.send(input).is_err() || closed {
                    return;
                }
            }
        })
        .map(|_| ())
}

/// Whether `line` was a response to one of the plugin's own host requests,
/// now handed to the request's waiter ([`HostRpc::try_route_response`]).
fn is_host_response(line: &[u8], host_rpc: &HostRpc) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    let text = text.trim();
    !text.is_empty()
        && serde_json::from_str::<serde_json::Value>(text)
            .is_ok_and(|v| host_rpc.try_route_response(&v))
}

/// Hear SIGTERM (and Ctrl-C) on a thread of its own and tell the main loop,
/// which then runs the shutdown steps before the process exits. A second one
/// while those run ends the process at once.
fn spawn_signal_watcher(tx: mpsc::Sender<Input>) {
    let spawned = std::thread::Builder::new()
        .name("aokie-signals".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    eprintln!("[aokie-plugin] signals will not be heard: {e}");
                    return;
                }
            };
            runtime.block_on(async move {
                #[cfg(unix)]
                let mut terminate = match tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                ) {
                    Ok(signal) => signal,
                    Err(e) => {
                        eprintln!("[aokie-plugin] SIGTERM will not be heard: {e}");
                        return;
                    }
                };
                let mut told = false;
                loop {
                    #[cfg(unix)]
                    let name = tokio::select! {
                        _ = terminate.recv() => "SIGTERM",
                        _ = tokio::signal::ctrl_c() => "Ctrl-C",
                    };
                    #[cfg(not(unix))]
                    let name = match tokio::signal::ctrl_c().await {
                        Ok(()) => "Ctrl-C",
                        Err(_) => return,
                    };
                    if told {
                        eprintln!("[aokie-plugin] {name} again: exiting now");
                        std::process::exit(1);
                    }
                    told = true;
                    if tx.send(Input::Signal(name)).is_err() {
                        return;
                    }
                }
            });
        });
    if let Err(e) = spawned {
        eprintln!("[aokie-plugin] signals will not be heard: {e}");
    }
}

fn main() {
    // The sealing key is asked of the Keychain (or Secret Service) now, on a
    // thread of its own, so that a prompt it shows comes at start rather
    // than in the middle of the first call, and nothing waits on it here.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    aokie_core::keychain_seal::prefetch_data_key();

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

    let (tx, rx) = mpsc::channel();
    if let Err(e) = spawn_stdin_reader(tx.clone(), plugin.host_rpc.clone()) {
        eprintln!("[aokie-plugin] fatal: stdin cannot be read: {e}");
        std::process::exit(1);
    }
    spawn_signal_watcher(tx);
    let mut sink = StdoutSink::new();

    // How the loop ended when it was not `plugin.shutdown` (which has run the
    // shutdown steps itself): the waits the steps get before the host's kill.
    let ended = loop {
        let Ok(input) = rx.recv() else {
            break Some(("stdin reader stopped", HOST_GRACE_RADIO_WAIT, HOST_GRACE_THREAD_WAIT));
        };
        match input {
            Input::Closed(why) => {
                if let Some(why) = why {
                    eprintln!("[aokie-plugin] stdin read error: {why}");
                }
                // Host closed stdin (OAIY does after its shutdown grace) →
                // shut down.
                break Some(("stdin closed", HOST_GRACE_RADIO_WAIT, HOST_GRACE_THREAD_WAIT));
            }
            Input::Signal(name) => break Some((name, SIGNAL_RADIO_WAIT, SIGNAL_THREAD_WAIT)),
            Input::TooLong => {
                let line = rpc::error_line(
                    None,
                    rpc::INVALID_REQUEST,
                    &format!("line exceeds {} bytes", rpc::MAX_LINE_BYTES),
                    None,
                );
                if sink.send_line(&line).is_err() {
                    break Some(("stdout closed", HOST_GRACE_RADIO_WAIT, HOST_GRACE_THREAD_WAIT));
                }
            }
            Input::Line(buf) => {
                let text = match std::str::from_utf8(&buf) {
                    Ok(t) => t.trim(),
                    Err(_) => {
                        let line =
                            rpc::error_line(None, rpc::PARSE_ERROR, "line is not UTF-8", None);
                        if sink.send_line(&line).is_err() {
                            break Some((
                                "stdout closed",
                                HOST_GRACE_RADIO_WAIT,
                                HOST_GRACE_THREAD_WAIT,
                            ));
                        }
                        continue;
                    }
                };
                if text.is_empty() {
                    continue;
                }
                let response = match rpc::parse_line(text) {
                    Ok(msg) => plugin.handle_rpc(msg, &mut sink),
                    Err(e) => Some(rpc::error_line(e.id.as_ref(), e.code, &e.message, None)),
                };
                if let Some(line) = response {
                    if sink.send_line(&line).is_err() {
                        // stdout gone → host died; shut down cleanly
                        break Some(("stdout closed", HOST_GRACE_RADIO_WAIT, HOST_GRACE_THREAD_WAIT));
                    }
                }
                if plugin.shutdown_requested {
                    break None; // plugin.shutdown was carried out above
                }
            }
        }
    };
    // Every other way out runs the same steps as plugin.shutdown: the radio
    // settles what it holds and closes the dongle (handing it back to the
    // system's driver off Windows) before the process goes.
    if let Some((why, radio_wait, thread_wait)) = ended {
        if !plugin.shutdown_requested {
            eprintln!("[aokie-plugin] {why}: shutting down");
            if let Err(error) = plugin.shutdown(radio_wait, thread_wait) {
                eprintln!("[aokie-plugin] shutdown: {error}; exiting all the same");
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
    fn host_responses_go_to_their_waiter_on_the_reader_thread() {
        let host = HostRpc::new();
        let (id, _line, waiter) = host.begin("flow.run", serde_json::json!({}));
        let response = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"ok":true}}}}"#);
        assert!(is_host_response(response.as_bytes(), &host));
        assert_eq!(waiter.try_recv().unwrap(), Ok(serde_json::json!({"ok": true})));
        // Requests and notifications are the main loop's, as is a line that
        // is not JSON (it gets the parse error).
        for other in [
            &br#"{"jsonrpc":"2.0","id":1,"method":"plugin.health"}"#[..],
            br#"{"jsonrpc":"2.0","method":"plugin.shutdown"}"#,
            b"not json",
            b"",
        ] {
            assert!(!is_host_response(other, &host));
        }
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
