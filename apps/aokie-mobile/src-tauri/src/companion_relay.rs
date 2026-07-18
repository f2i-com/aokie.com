//! FormLogic-hosted Companion relay carrier (mobile side).
//!
//! An authenticated frame mailbox standing in for the v2 signalling WebSocket:
//! the Companion POSTs frames addressed to the plugin and reads frames
//! addressed to itself off a Server-Sent Events stream. The relay never
//! interprets a frame — it stores and forwards opaque JSON — so every protocol
//! decision still happens in [`crate::realtime_v2`], exactly as it does over the
//! socket.
//!
//! This is a deliberate port of the plugin's proven carrier
//! (`crates/aokie-plugin/src/companion_relay.rs`), reduced for the mobile side.
//! Two properties of the hosted mailbox drive its shape:
//!
//! * **The stream has a hard 300-second lifetime.** It ends cleanly with an
//!   `event: end` marker and the client resumes from its cursor. That is a
//!   routine re-open, NOT an outage: surfacing it as a disconnect would tear
//!   down the session every five minutes.
//! * **There is no gateway process in the middle.** The plugin routes to many
//!   Companions and therefore needs a device→party map; a Companion has exactly
//!   ONE destination, and the relay itself enforces that a mobile party may
//!   address only `"plugin"`. So the plugin's routing table, greet loop and
//!   undeliverable throttle have no mobile counterpart and are deliberately
//!   absent rather than ported dormant.
//!
//! ## Cancellation
//!
//! [`RelayChannel::recv`] is driven as a `tokio::select!` arm in the session
//! loop, so its future is dropped whenever a sibling arm wins — several times a
//! second, since native call actions tick at 200ms. That is safe here by
//! construction, and the reasoning is worth stating because it is not obvious:
//!
//! * The only cancellable await on the receive path is `Response::chunk()`,
//!   whose decode state lives in the response body rather than the future, so a
//!   dropped poll resumes rather than loses bytes. This is the same
//!   `timeout(tick, chunk())` shape the plugin runs against the live relay.
//! * Every state mutation that follows a completed read — parser push, cursor
//!   advance, queueing a decoded frame — happens with no intervening `.await`,
//!   so a frame is either fully queued in [`RelayChannel::inbound`] (and popped
//!   by the next call) or never parsed at all. A cancelled `recv` cannot strand
//!   a frame between the two.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use aokie_protocol::v2::EndpointChallengeFrame;
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use url::Url;

use crate::managed_auth::RelayEndpoints;

/// The only party a Companion may address, and the only one it accepts frames
/// from. The relay derives our own party from the bearer's verified claims and
/// refuses any other destination, so this is a local mirror of a server-side
/// rule rather than the rule itself.
const PLUGIN_PARTY: &str = "plugin";

/// The challenge is a small document on an already-authenticated route.
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Frame POSTs are small and must not wedge the session loop.
const POST_TIMEOUT: Duration = Duration::from_secs(10);
/// Opening the stream is a plain request; only the BODY is long-lived, so the
/// handshake gets a bounded budget while the read itself never times out as a
/// whole request.
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Comfortably beyond the relay's 15-second heartbeat comment cadence: past
/// this with no bytes at all the stream is dead rather than idle.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// Pause before re-opening a stream that failed, so a relay outage cannot turn
/// into a request loop. A clean end-of-lifetime re-opens immediately.
const STREAM_REOPEN_DELAY: Duration = Duration::from_secs(1);
/// Consecutive stream failures absorbed as "quiet" before the session is told.
/// Frames survive server-side for 120s and we resume from a cursor, so a short
/// gap costs nothing — while a torn-down session drops a live call.
const MAX_ABSORBED_STREAM_FAILURES: u32 = 3;
/// `AokieCompanionRelayService::MAX_FRAMES_PER_POST`.
const MAX_FRAMES_PER_POST: usize = 64;
/// The relay's per-frame ceiling. Refused locally so an oversized frame fails
/// honestly here instead of as an opaque 4xx.
const MAX_RELAY_FRAME_BYTES: usize = 32 * 1024;
/// Attempts for one frame batch before it is dropped (429) or reported.
const POST_ATTEMPTS: u32 = 3;
const POST_RETRY_DELAY: Duration = Duration::from_millis(250);
/// A single SSE event that never terminates is a broken stream, not a big one.
const MAX_STREAM_BUFFER_BYTES: usize = 1024 * 1024;
/// Pages walked while priming a new session's cursor. The relay caps one fetch
/// at `MAX_FETCH_LIMIT` (128) rows while an app mailbox may hold
/// `MAX_LIVE_FRAMES_PER_APP` (512), so a single page cannot reach the tail.
const TAIL_PRIME_PAGES: u32 = 8;
/// How long a stream must stay open before a read counts as proof the carrier
/// works. The relay flushes `retry:` and `: connected` the instant a stream
/// opens, so an immediate read proves only that the preamble arrived — well
/// under the 15-second heartbeat cadence a healthy idle stream keeps up.
const STREAM_PROVEN_AFTER: Duration = Duration::from_secs(5);

/// One decoded relay stream event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayStreamEvent {
    /// A stored frame addressed to this party.
    Frame {
        seq: u64,
        from: String,
        frame: String,
    },
    /// The relay's hard-lifetime marker: resume from `seq` on a fresh stream.
    End { seq: Option<u64> },
}

/// A failure while opening the carrier.
///
/// Only the connect path needs this shape: the session treats a rejected
/// admission differently from an unreachable relay, and everything after the
/// open is a plain message the session either absorbs or surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayConnectError {
    pub(crate) message: String,
    /// The bearer itself is finished, rather than the relay being unreachable.
    pub(crate) admission_rejected: bool,
}

/// What one receive tick produced for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayReceive {
    /// Nothing happened this tick.
    Idle,
    /// The carrier proved liveness without producing a session frame: a
    /// heartbeat comment, an end-of-lifetime marker, or a frame from a party
    /// this session does not accept.
    ///
    /// Distinct from [`Self::Idle`] because the session's inbound freshness
    /// timer must not fire on a healthy but quiet relay — over the socket the
    /// gateway's pings keep it fed, and this is the relay's equivalent.
    Alive,
    /// A frame for the protocol layer.
    Frame(String),
}

/// Incremental Server-Sent Events reader.
///
/// Pure and byte-oriented so the wire shape can be locked in tests without a
/// live stream: chunks arrive split at arbitrary offsets, heartbeat comments
/// carry no event, and the end-of-lifetime marker has to be distinguishable
/// from a failure.
#[derive(Default)]
pub(crate) struct SseParser {
    buffer: String,
}

impl SseParser {
    pub(crate) fn push(&mut self, chunk: &str) -> Vec<RelayStreamEvent> {
        // Normalising line endings up front lets one separator search cover
        // both CRLF and LF streams.
        self.buffer.push_str(&chunk.replace("\r\n", "\n"));
        let mut events = Vec::new();
        while let Some(boundary) = self.buffer.find("\n\n") {
            let block = self.buffer[..boundary].to_string();
            self.buffer.drain(..boundary + 2);
            if let Some(event) = parse_sse_block(&block) {
                events.push(event);
            }
        }
        // Complete events are drained FIRST, so only an event that never
        // terminates can sit above the ceiling — a broken stream, not a big
        // one. Clearing before the drain would throw away whole frames the
        // cursor has not advanced past, and nothing re-reads them: the carrier
        // only resumes from `last_seq`, which those frames never reached.
        if self.buffer.len() > MAX_STREAM_BUFFER_BYTES {
            eprintln!(
                "[AokieCompanion][relay] discarded an SSE event that exceeded the buffer ceiling"
            );
            self.buffer.clear();
        }
        events
    }
}

fn parse_sse_block(block: &str) -> Option<RelayStreamEvent> {
    let mut id = None;
    let mut name = None;
    let mut data = String::new();
    for line in block.split('\n') {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, raw) = match line.split_once(':') {
            Some((field, raw)) => (field, raw.strip_prefix(' ').unwrap_or(raw)),
            None => (line, ""),
        };
        match field {
            "id" => id = raw.parse::<u64>().ok(),
            "event" => name = Some(raw.to_string()),
            "data" => {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(raw);
            }
            _ => {}
        }
    }
    match name.as_deref() {
        Some("end") => Some(RelayStreamEvent::End { seq: id }),
        Some("frame") => {
            let payload: Value = serde_json::from_str(&data).ok()?;
            let seq = payload.get("seq")?.as_u64()?;
            let from = payload.get("from")?.as_str()?.to_string();
            // The stored document is re-serialised rather than passed through
            // verbatim; every signature the session checks is recomputed from
            // parsed claims, so member order carries no meaning.
            let frame = serde_json::to_string(payload.get("frame")?).ok()?;
            Some(RelayStreamEvent::Frame { seq, from, frame })
        }
        _ => None,
    }
}

pub(crate) struct RelayChannel {
    client: Client,
    endpoints: RelayEndpoints,
    token: String,
    app_id: String,
    device_id: String,
    inbound: VecDeque<String>,
    stream: Option<Response>,
    parser: SseParser,
    /// A character split across two reads. Never more than three bytes: every
    /// completed character is drained on the read that finishes it.
    pending_bytes: Vec<u8>,
    last_seq: u64,
    last_stream_read: Instant,
    /// When the live stream opened, so the relay's immediate `retry:` /
    /// `: connected` preamble cannot masquerade as a working carrier.
    stream_opened_at: Instant,
    reopen_not_before: Instant,
    stream_failures: u32,
}

impl RelayChannel {
    /// Fetch this session's endpoint challenge.
    ///
    /// The caller validates it and signs the hello through the same
    /// `endpoint_handshake` the socket runs, so the relay and the socket
    /// provably produce the same proof from the same document.
    pub(crate) async fn connect(
        endpoints: &RelayEndpoints,
        token: &str,
        app_id: &str,
        device_id: &str,
    ) -> Result<(Self, EndpointChallengeFrame), RelayConnectError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| RelayConnectError {
                message: "could not initialise the Companion relay TLS client".into(),
                admission_rejected: false,
            })?;
        let mut channel = Self {
            client,
            endpoints: endpoints.clone(),
            token: token.to_string(),
            app_id: app_id.to_string(),
            device_id: device_id.to_string(),
            inbound: VecDeque::new(),
            stream: None,
            parser: SseParser::default(),
            pending_bytes: Vec::new(),
            last_seq: 0,
            last_stream_read: Instant::now(),
            stream_opened_at: Instant::now(),
            reopen_not_before: Instant::now(),
            stream_failures: 0,
        };
        let challenge = channel.fetch_challenge().await?;
        channel.last_seq = channel.tail_cursor().await?;
        eprintln!(
            "[AokieCompanion][relay] challenge accepted; app={} device={} resume_from={}",
            channel.app_id, channel.device_id, channel.last_seq
        );
        Ok((channel, challenge))
    }

    /// Where a NEW session starts reading.
    ///
    /// The mailbox keeps frames for 120 seconds, so a fresh channel that began
    /// at zero would replay a previous session's traffic: stale snapshots and
    /// lease decisions bound to epochs this session never saw, which the
    /// protocol layer rejects, which tears the session down, which re-reads the
    /// same backlog on the next attempt — a crash loop that only ends when the
    /// TTL does. Starting at the current tail is what the socket gateway does
    /// implicitly by having no backlog at all.
    ///
    /// A rotation must NOT re-prime: [`Self::adopt_routing_from`] restores the
    /// predecessor's cursor so nothing that arrived mid-handshake is skipped.
    ///
    /// The reported `lastSeq` is the tail of ONE page — the relay caps a fetch
    /// at 128 rows while an app mailbox holds up to 512 — so priming has to
    /// page forward until the cursor stops moving. Stopping at the first page
    /// would leave the remainder of the previous session's backlog in front of
    /// the new stream, which is exactly the replay this exists to prevent.
    async fn tail_cursor(&self) -> Result<u64, RelayConnectError> {
        let mut cursor = 0;
        for _ in 0..TAIL_PRIME_PAGES {
            let advanced = self.tail_cursor_page(cursor).await?;
            if advanced <= cursor {
                break;
            }
            cursor = advanced;
        }
        Ok(cursor)
    }

    /// One page of [`Self::tail_cursor`]: the highest cursor the relay will
    /// report for frames after `since`, or `since` itself when none remain.
    async fn tail_cursor_page(&self, since: u64) -> Result<u64, RelayConnectError> {
        let mut url = Url::parse(&self.endpoints.frames_url).map_err(|_| RelayConnectError {
            message: "Companion relay framesUrl is invalid".into(),
            admission_rejected: false,
        })?;
        url.query_pairs_mut()
            .append_pair("since", &since.to_string())
            .append_pair("wait", "0");
        let response = self
            .request(reqwest::Method::GET, url.as_str())
            .map_err(connect_setup_error)?
            .timeout(POST_TIMEOUT)
            .send()
            .await
            .map_err(|error| connect_transport_error("frames", &error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(connect_status_error("frames", status));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|error| connect_transport_error("frames", &error))?;
        Ok(body
            .get("lastSeq")
            .and_then(Value::as_u64)
            .unwrap_or(since)
            .max(since))
    }

    /// Carry a rotating session's continuity onto its replacement channel.
    ///
    /// The endpoint authority is unchanged across an admission rotation (the
    /// session asserts it), so the cursor MUST come across — otherwise the
    /// replacement re-reads everything the predecessor already handed to the
    /// session, inside the mailbox's 120s TTL.
    ///
    /// ⚠️ Known consequence of where the session calls this. A replacement runs
    /// its own `initial_sync` before adoption, so it may consume ONE frame from
    /// its freshly primed tail; adopting the predecessor's lower cursor then
    /// re-reads that frame. The alternative — adopting first — would mean
    /// syncing against a cursor whose frames the replacement has not opened a
    /// stream for yet. One replayed authoritative frame per rotation is the
    /// cheaper error: the session is explicitly built to ignore an
    /// already-applied authoritative frame, whereas SKIPPING the predecessor's
    /// unread range would silently lose state.
    pub(crate) fn adopt_routing_from(&mut self, previous: &mut Self) {
        self.last_seq = previous.last_seq;
        self.inbound = std::mem::take(&mut previous.inbound);
    }

    async fn fetch_challenge(&self) -> Result<EndpointChallengeFrame, RelayConnectError> {
        let response = self
            .request(reqwest::Method::GET, &self.endpoints.challenge_url)
            .map_err(connect_setup_error)?
            .timeout(CHALLENGE_TIMEOUT)
            .send()
            .await
            .map_err(|error| connect_transport_error("challenge", &error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(connect_status_error("challenge", status));
        }
        let body = response
            .text()
            .await
            .map_err(|error| connect_transport_error("challenge", &error))?;
        serde_json::from_str(&body).map_err(|_| RelayConnectError {
            message: "Companion endpoint challenge is malformed".into(),
            admission_rejected: false,
        })
    }

    /// Post one frame to the plugin.
    ///
    /// Returns `false` on failure to match the socket path's `send_text`, whose
    /// callers treat a false as "break the session".
    pub(crate) async fn send_text(&mut self, encoded: &str) -> bool {
        if encoded.len() > MAX_RELAY_FRAME_BYTES {
            eprintln!("[AokieCompanion][relay] refused a frame above the relay's size ceiling");
            return false;
        }
        self.post_frames(PLUGIN_PARTY, &[encoded]).await.is_ok()
    }

    pub(crate) async fn recv(&mut self, tick: Duration) -> Result<RelayReceive, String> {
        if let Some(frame) = self.inbound.pop_front() {
            return Ok(RelayReceive::Frame(frame));
        }
        if self.stream.is_none() {
            let now = Instant::now();
            if now < self.reopen_not_before {
                // Hold the caller's cadence. Returning immediately would spin
                // the session loop at full speed for the whole reopen delay.
                tokio::time::sleep(tick.min(self.reopen_not_before - now)).await;
                return Ok(RelayReceive::Idle);
            }
            if let Err(error) = self.open_stream().await {
                self.reopen_not_before = Instant::now() + STREAM_REOPEN_DELAY;
                return self
                    .note_stream_failure(error)
                    .map(|()| RelayReceive::Idle);
            }
        }
        let Some(stream) = self.stream.as_mut() else {
            return Ok(RelayReceive::Idle);
        };
        match tokio::time::timeout(tick, stream.chunk()).await {
            Err(_) => {
                if self.last_stream_read.elapsed() < STREAM_IDLE_TIMEOUT {
                    return Ok(RelayReceive::Idle);
                }
                self.restart_stream(STREAM_REOPEN_DELAY);
                self.note_stream_failure(
                    "Companion relay stream went silent past its heartbeat window".into(),
                )
                .map(|()| RelayReceive::Idle)
            }
            Ok(Ok(Some(bytes))) => {
                self.last_stream_read = Instant::now();
                let text = self.decode_stream_bytes(&bytes);
                let events = self.parser.push(&text);
                self.note_stream_progress(!events.is_empty());
                self.absorb(events);
                // Bytes off the wire are carrier liveness even when they carry
                // no session frame: an idle relay heartbeats every 15 seconds
                // and that has to keep the session's freshness timer fed.
                Ok(self
                    .inbound
                    .pop_front()
                    .map_or(RelayReceive::Alive, RelayReceive::Frame))
            }
            Ok(Ok(None)) => {
                // The relay signs off with `event: end`; a body that just stops
                // is abnormal, so resume from the cursor but count it.
                self.restart_stream(STREAM_REOPEN_DELAY);
                self.note_stream_failure(
                    "Companion relay stream ended without a resume marker".into(),
                )
                .map(|()| RelayReceive::Idle)
            }
            Ok(Err(error)) => {
                let failure = relay_transport_error("stream", &error);
                self.restart_stream(STREAM_REOPEN_DELAY);
                self.note_stream_failure(failure)
                    .map(|()| RelayReceive::Idle)
            }
        }
    }

    /// Decode stream bytes, carrying a character split across a read.
    ///
    /// ⚠️ NOT `from_utf8_lossy` per chunk. `Response::chunk()` hands back
    /// whatever the transport produced, and a boundary lands wherever TCP put
    /// it — including the middle of a multi-byte character. Decoding each chunk
    /// in isolation replaces both halves with U+FFFD, and because the damage is
    /// inside a JSON string the frame still parses: the corruption is silent
    /// and reaches the operator as mangled text. Captions and caller names are
    /// exactly the fields carrying non-ASCII, so this is the common case for
    /// any non-English line, not an edge one.
    fn decode_stream_bytes(&mut self, bytes: &[u8]) -> String {
        self.pending_bytes.extend_from_slice(bytes);
        let split = match std::str::from_utf8(&self.pending_bytes) {
            Ok(_) => self.pending_bytes.len(),
            // `error_len() == None` means the tail is INCOMPLETE rather than
            // invalid — the rest of the character is still in flight, so carry
            // it to the next read.
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            // Genuinely invalid bytes can never be completed by anything that
            // follows, so they are consumed here; carrying them would wedge the
            // decoder on every subsequent read.
            Err(_) => self.pending_bytes.len(),
        };
        let head: Vec<u8> = self.pending_bytes.drain(..split).collect();
        String::from_utf8_lossy(&head).into_owned()
    }

    /// Clear the failure streak only when a read actually proves the carrier
    /// works.
    ///
    /// A successful OPEN proves nothing, and neither does the first byte: the
    /// relay flushes `retry:` and `: connected` the instant a stream opens, so
    /// a carrier that greets and dies on every cycle would reset the streak
    /// forever and retry in silence behind a Connected status — exactly the
    /// state [`MAX_ABSORBED_STREAM_FAILURES`] exists to escape. A session frame
    /// proves delivery; otherwise the stream has to outlive the preamble, which
    /// a healthy idle stream does on its 15-second heartbeat.
    fn note_stream_progress(&mut self, delivered_event: bool) {
        if delivered_event || self.stream_opened_at.elapsed() >= STREAM_PROVEN_AFTER {
            self.stream_failures = 0;
        }
    }

    /// Absorb a stream fault as quiet time, or surface it once the carrier has
    /// clearly stopped working.
    ///
    /// Frames survive 120 seconds server-side and every re-open resumes from a
    /// cursor, so a brief gap costs nothing. A stream that keeps failing is a
    /// different thing entirely and has to reach the session: retrying forever
    /// in silence would leave the status reading Connected while no signalling
    /// moves at all.
    fn note_stream_failure(&mut self, error: String) -> Result<(), String> {
        self.stream_failures = self.stream_failures.saturating_add(1);
        if self.stream_failures > MAX_ABSORBED_STREAM_FAILURES {
            return Err(error);
        }
        eprintln!(
            "[AokieCompanion][relay] stream retry {}: {error}",
            self.stream_failures
        );
        Ok(())
    }

    fn absorb(&mut self, events: Vec<RelayStreamEvent>) {
        for event in events {
            match event {
                RelayStreamEvent::Frame { seq, from, frame } => {
                    self.last_seq = self.last_seq.max(seq);
                    // A Companion has exactly one peer. The relay already
                    // refuses to deliver anything a mobile party is not
                    // entitled to, so this is defence in depth rather than the
                    // access rule — but it is the line that keeps a frame from
                    // another Companion out of the protocol layer.
                    if from != PLUGIN_PARTY {
                        eprintln!(
                            "[AokieCompanion][relay] ignored a frame from a party other than the plugin"
                        );
                        continue;
                    }
                    self.inbound.push_back(frame);
                }
                RelayStreamEvent::End { seq } => {
                    if let Some(seq) = seq {
                        self.last_seq = self.last_seq.max(seq);
                    }
                    // The relay's hard lifetime, not an outage. Re-open at
                    // once: no failure count, no reconnect, no phase change —
                    // the session never learns this happened.
                    self.restart_stream(Duration::ZERO);
                }
            }
        }
    }

    fn restart_stream(&mut self, delay: Duration) {
        self.stream = None;
        self.parser = SseParser::default();
        // A half-read character belongs to the stream that is going away; the
        // replacement resumes from `last_seq` and re-reads whole frames.
        self.pending_bytes.clear();
        self.last_stream_read = Instant::now();
        self.reopen_not_before = Instant::now() + delay;
    }

    async fn open_stream(&mut self) -> Result<(), String> {
        let mut url = Url::parse(&self.endpoints.stream_url)
            .map_err(|_| "Companion relay streamUrl is invalid".to_string())?;
        url.query_pairs_mut()
            .append_pair("since", &self.last_seq.to_string());
        let response = tokio::time::timeout(
            STREAM_OPEN_TIMEOUT,
            self.request(reqwest::Method::GET, url.as_str())?
                .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
                .send(),
        )
        .await
        .map_err(|_| "Companion relay stream did not open in time".to_string())?
        .map_err(|error| relay_transport_error("stream", &error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(relay_status_error("stream", status));
        }
        self.stream = Some(response);
        self.last_stream_read = Instant::now();
        self.stream_opened_at = Instant::now();
        Ok(())
    }

    async fn post_frames(&self, party: &str, frames: &[&str]) -> Result<(), String> {
        if frames.is_empty() {
            return Ok(());
        }
        for chunk in frames.chunks(MAX_FRAMES_PER_POST) {
            self.post_batch(party, chunk).await?;
        }
        Ok(())
    }

    async fn post_batch(&self, party: &str, frames: &[&str]) -> Result<(), String> {
        let body = relay_post_body(party, frames);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = self
                .request(reqwest::Method::POST, &self.endpoints.frames_url)?
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .timeout(POST_TIMEOUT)
                .body(body.clone())
                .send()
                .await;
            let error = match outcome {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => {
                    let status = response.status();
                    if status == StatusCode::TOO_MANY_REQUESTS && attempt < POST_ATTEMPTS {
                        tokio::time::sleep(POST_RETRY_DELAY).await;
                        continue;
                    }
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        // Systemic backpressure: the app mailbox is full
                        // because nothing is draining it.
                        //
                        // ⚠️ REPORTED AS A FAILURE, unlike the plugin's carrier
                        // this is otherwise a port of. The plugin's outbound
                        // traffic is authoritative STATE, which it republishes
                        // on the next tick, so swallowing a drop there costs a
                        // refresh. A Companion's outbound traffic is USER
                        // COMMANDS — answer, hang up, revoke a lease, confirm
                        // an end-caller challenge — and `send_text`'s callers
                        // read a `true` as proof of delivery: the native call
                        // action path reports `lease_request_sent` to Android
                        // telecom and marks the lease delivered, and a queued
                        // Tauri command resolves `Ok(())` to the UI. Returning
                        // success here would tell the operator their command
                        // landed while it was discarded, and the lease would
                        // then wait forever for a decision no one was asked
                        // for. Failing honestly reconnects and re-syncs.
                        eprintln!(
                            "[AokieCompanion][relay] mailbox is full; {} frame(s) were not delivered",
                            frames.len()
                        );
                        return Err(format!(
                            "Companion relay mailbox is full ({status})"
                        ));
                    }
                    relay_status_error("frames", status)
                }
                Err(error) => relay_transport_error("frames", &error),
            };
            if attempt >= POST_ATTEMPTS {
                return Err(error);
            }
            tokio::time::sleep(POST_RETRY_DELAY).await;
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, String> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| "Companion admission token is invalid".to_string())?;
        authorization.set_sensitive(true);
        Ok(self
            .client
            .request(method, url)
            .header(AUTHORIZATION, authorization)
            .header(
                "x-aokie-app-id",
                HeaderValue::from_str(&self.app_id)
                    .map_err(|_| "Companion app identity is invalid".to_string())?,
            )
            .header(
                "x-aokie-device-id",
                HeaderValue::from_str(&self.device_id)
                    .map_err(|_| "Companion device identity is invalid".to_string())?,
            ))
    }
}

/// Assemble the POST body around already-serialised frames.
///
/// The frames are embedded verbatim rather than decoded and re-encoded: the
/// relay preserves member order end to end, so the document the plugin reads is
/// the document this session produced — which matters because an endpoint proof
/// is verified over bytes the sender chose.
fn relay_post_body(party: &str, frames: &[&str]) -> String {
    let mut body = String::from("{\"to\":");
    body.push_str(&Value::String(party.to_string()).to_string());
    body.push_str(",\"frames\":[");
    for (index, frame) in frames.iter().enumerate() {
        if index > 0 {
            body.push(',');
        }
        body.push_str(frame);
    }
    body.push_str("]}");
    body
}

fn connect_setup_error(message: String) -> RelayConnectError {
    RelayConnectError {
        message,
        admission_rejected: false,
    }
}

fn connect_transport_error(stage: &str, error: &reqwest::Error) -> RelayConnectError {
    RelayConnectError {
        message: relay_transport_error(stage, error),
        admission_rejected: false,
    }
}

/// 401 and 403 are the two the session must tell apart from "unreachable":
/// the bearer is finished, or the owner turned the relay off for this app.
fn connect_status_error(stage: &str, status: StatusCode) -> RelayConnectError {
    RelayConnectError {
        message: relay_status_error(stage, status),
        admission_rejected: matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
    }
}

fn relay_transport_error(stage: &str, error: &reqwest::Error) -> String {
    // Never render the error itself: reqwest's Display includes the full URL.
    let detail = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not be reached"
    } else if error.is_decode() {
        "returned an unreadable response"
    } else {
        "failed"
    };
    format!("Companion relay {stage} {detail}")
}

fn relay_status_error(stage: &str, status: StatusCode) -> String {
    match status {
        // The admission behind the bearer is finished; the session rotates.
        StatusCode::UNAUTHORIZED => {
            format!("Companion relay {stage} rejected the admission ({status})")
        }
        // An owner turned the service off, or the app is gone. Retrying at
        // socket cadence would achieve nothing.
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::SERVICE_UNAVAILABLE => {
            format!("Companion relay {stage} is unavailable for this app ({status})")
        }
        _ => format!("Companion relay {stage} failed ({status})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_event(seq: u64, from: &str, frame: &str) -> String {
        format!("id: {seq}\nevent: frame\ndata: {{\"seq\":{seq},\"from\":\"{from}\",\"frame\":{frame}}}\n\n")
    }

    #[test]
    fn stream_events_survive_chunks_that_split_mid_event() {
        let mut parser = SseParser::default();
        let encoded = frame_event(7, "plugin", "{\"kind\":\"plugin_idle\",\"appId\":\"app_a\"}");
        let (head, tail) = encoded.split_at(encoded.len() / 2);

        // A chunk boundary inside an event yields nothing until the event ends.
        assert!(parser.push(head).is_empty());
        let events = parser.push(tail);

        let [RelayStreamEvent::Frame { seq, from, frame }] = events.as_slice() else {
            panic!("the split event decodes to exactly one frame, got {events:?}");
        };
        assert_eq!(*seq, 7);
        assert_eq!(from, "plugin");
        // The frame is handed on SEMANTICALLY, not byte for byte: re-serialising
        // through serde_json::Value sorts members. Safe in this direction
        // because every signature the protocol checks is recomputed from parsed
        // claims, never over transport bytes. (Outbound is the opposite:
        // relay_post_body embeds our own frames verbatim — locked by
        // post_body_embeds_frames_verbatim_and_addresses_the_plugin.)
        assert_eq!(
            serde_json::from_str::<Value>(frame).unwrap(),
            serde_json::json!({"kind": "plugin_idle", "appId": "app_a"})
        );
    }

    #[test]
    fn heartbeat_comments_and_retry_hints_carry_no_session_frame() {
        let mut parser = SseParser::default();

        assert!(parser
            .push("retry: 2000\n\n: connected\n\n: keepalive\n\n")
            .is_empty());

        // The stream stays usable: a real event after the comments still lands.
        let events = parser.push(&frame_event(3, "plugin", "{\"kind\":\"rtc_signal\"}"));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn end_of_lifetime_is_a_resume_marker_rather_than_a_failure() {
        let mut parser = SseParser::default();

        let events = parser.push("id: 42\nevent: end\ndata: {}\n\n");

        // `End` exists precisely so the carrier can re-open silently: mapping
        // the relay's 300s lifetime onto a disconnect would tear the session
        // down every five minutes.
        assert_eq!(events, vec![RelayStreamEvent::End { seq: Some(42) }]);
    }

    #[test]
    fn crlf_streams_and_multi_line_data_decode_identically() {
        let mut parser = SseParser::default();

        let events = parser.push("id: 9\r\nevent: frame\r\ndata: {\"seq\":9,\"from\":\"plugin\",\r\ndata: \"frame\":{\"kind\":\"plugin_idle\"}}\r\n\r\n");

        assert_eq!(
            events,
            vec![RelayStreamEvent::Frame {
                seq: 9,
                from: "plugin".into(),
                frame: "{\"kind\":\"plugin_idle\"}".into(),
            }]
        );
    }

    #[test]
    fn a_complete_event_is_never_discarded_by_the_buffer_ceiling() {
        let mut parser = SseParser::default();

        // One oversized event that never terminates is a broken stream and is
        // dropped — but it must not take finished frames with it. Nothing
        // re-reads a discarded frame: the carrier resumes from `last_seq`, and
        // a frame parsed out of the buffer never reached it.
        let mut chunk = frame_event(11, "plugin", "{\"kind\":\"plugin_idle\"}");
        chunk.push_str("event: frame\ndata: ");
        chunk.push_str(&"x".repeat(MAX_STREAM_BUFFER_BYTES + 1));

        let events = parser.push(&chunk);

        assert_eq!(
            events,
            vec![RelayStreamEvent::Frame {
                seq: 11,
                from: "plugin".into(),
                frame: "{\"kind\":\"plugin_idle\"}".into(),
            }],
            "the finished frame survives the oversized one behind it"
        );
        // The runaway event itself is gone, so the stream stays usable.
        assert!(parser.buffer.is_empty());
    }

    /// `Response::chunk()` splits wherever TCP did, including mid-character.
    /// Decoding each chunk on its own turns both halves into U+FFFD, and since
    /// the damage lands inside a JSON string the frame still parses — so a
    /// caption or caller name reaches the operator silently mangled.
    #[test]
    fn a_character_split_across_two_reads_is_not_corrupted() {
        let mut channel = test_channel();
        let encoded = frame_event(
            12,
            "plugin",
            "{\"kind\":\"plugin_idle\",\"caller\":\"Åsa 日本 🙂\"}",
        );
        let bytes = encoded.as_bytes();

        // Split inside the four-byte emoji, which is where a real read boundary
        // is just as likely to fall as anywhere else.
        let split = encoded.find('🙂').expect("fixture has the emoji") + 2;
        let head = channel.decode_stream_bytes(&bytes[..split]);
        let mut events = channel.parser.push(&head);
        assert!(events.is_empty(), "half an event yields nothing yet");
        let tail = channel.decode_stream_bytes(&bytes[split..]);
        events = channel.parser.push(&tail);

        let [RelayStreamEvent::Frame { frame, .. }] = events.as_slice() else {
            panic!("the split event decodes to exactly one frame, got {events:?}");
        };
        let decoded: Value = serde_json::from_str(frame).expect("the frame is valid JSON");
        assert_eq!(decoded["caller"], "Åsa 日本 🙂");
        // Nothing is left behind once the character is complete.
        assert!(channel.pending_bytes.is_empty());
    }

    /// Invalid bytes can never be completed by whatever follows, so they must be
    /// consumed rather than carried — a carry that never drains would wedge the
    /// decoder for the life of the stream.
    #[test]
    fn invalid_bytes_do_not_wedge_the_decoder() {
        let mut channel = test_channel();

        let text = channel.decode_stream_bytes(&[0xff, 0xfe]);

        assert!(!text.is_empty(), "invalid bytes are consumed lossily");
        assert!(channel.pending_bytes.is_empty());
        // The stream stays usable: a real event straight after still decodes.
        let next = frame_event(13, "plugin", "{\"kind\":\"plugin_idle\"}");
        let decoded = channel.decode_stream_bytes(next.as_bytes());
        let events = channel.parser.push(&decoded);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn post_body_embeds_frames_verbatim_and_addresses_the_plugin() {
        let body = relay_post_body(
            PLUGIN_PARTY,
            &["{\"kind\":\"mobile_hello\"}", "{\"kind\":\"rtc_signal\"}"],
        );

        assert_eq!(
            body,
            "{\"to\":\"plugin\",\"frames\":[{\"kind\":\"mobile_hello\"},{\"kind\":\"rtc_signal\"}]}"
        );
        let decoded: Value = serde_json::from_str(&body).expect("relay body is valid JSON");
        assert_eq!(decoded["to"], "plugin");
        assert_eq!(decoded["frames"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn frames_from_any_party_other_than_the_plugin_are_ignored() {
        let mut channel = test_channel();

        channel.absorb(vec![
            RelayStreamEvent::Frame {
                seq: 4,
                from: "mobile:another_companion".into(),
                frame: "{\"kind\":\"plugin_idle\"}".into(),
            },
            RelayStreamEvent::Frame {
                seq: 5,
                from: "plugin".into(),
                frame: "{\"kind\":\"plugin_idle\",\"appId\":\"app_a\"}".into(),
            },
        ]);

        assert_eq!(channel.inbound.len(), 1);
        assert_eq!(
            channel.inbound.front().map(String::as_str),
            Some("{\"kind\":\"plugin_idle\",\"appId\":\"app_a\"}")
        );
        // The cursor still advances past a skipped row so it is never re-read.
        assert_eq!(channel.last_seq, 5);
    }

    #[test]
    fn a_broken_stream_is_absorbed_briefly_then_reported() {
        let mut channel = test_channel();

        // A short gap is quiet time: frames live 120s server-side and every
        // re-open resumes from the cursor, so failing the session here would
        // drop a live call over nothing.
        for attempt in 1..=MAX_ABSORBED_STREAM_FAILURES {
            assert!(
                channel
                    .note_stream_failure("relay stream broke".into())
                    .is_ok(),
                "failure {attempt} should still be absorbed"
            );
        }

        // A carrier that never recovers must reach the session rather than
        // looping in silence behind a Connected status.
        assert!(channel
            .note_stream_failure("relay stream broke".into())
            .is_err());

        channel.stream_failures = 0;
        assert!(channel
            .note_stream_failure("relay stream broke".into())
            .is_ok());
    }

    #[test]
    fn the_relays_open_preamble_cannot_pass_for_a_working_carrier() {
        let mut channel = test_channel();
        channel.stream_failures = MAX_ABSORBED_STREAM_FAILURES;

        // The relay flushes `retry:` and `: connected` the instant a stream
        // opens. A carrier that greets and dies therefore reads bytes on every
        // cycle: if that cleared the streak, the escalation could never fire and
        // the session would retry forever behind a Connected status.
        channel.stream_opened_at = Instant::now();
        channel.note_stream_progress(false);
        assert_eq!(channel.stream_failures, MAX_ABSORBED_STREAM_FAILURES);
        assert!(channel
            .note_stream_failure("relay stream broke".into())
            .is_err());

        // A delivered session frame is proof, whenever it lands.
        channel.stream_failures = MAX_ABSORBED_STREAM_FAILURES;
        channel.note_stream_progress(true);
        assert_eq!(channel.stream_failures, 0);

        // So is a stream that simply outlives the preamble — which a healthy
        // idle stream does on its 15-second heartbeat comment.
        channel.stream_failures = MAX_ABSORBED_STREAM_FAILURES;
        channel.stream_opened_at = Instant::now() - (STREAM_PROVEN_AFTER + Duration::from_secs(1));
        channel.note_stream_progress(false);
        assert_eq!(channel.stream_failures, 0);
    }

    #[test]
    fn an_admission_rotation_carries_the_cursor_and_undelivered_frames() {
        let mut previous = test_channel();
        previous.absorb(vec![RelayStreamEvent::Frame {
            seq: 118,
            from: "plugin".into(),
            frame: "{\"kind\":\"plugin_idle\",\"appId\":\"app_a\"}".into(),
        }]);

        let mut next = test_channel();
        // A replacement primes its own cursor at the tail; adoption must
        // overwrite it, because the predecessor's position is the one the
        // session has actually consumed up to.
        next.last_seq = 900;
        next.adopt_routing_from(&mut previous);

        // Without the cursor the replacement re-reads everything still inside
        // the 120s mailbox TTL, replaying stale authoritative state.
        assert_eq!(next.last_seq, 118);
        // A frame the predecessor had read but not yet handed over is not lost.
        assert_eq!(next.inbound.len(), 1);
        assert!(previous.inbound.is_empty());
    }

    /// The one test that actually executes relay HTTP requests.
    #[tokio::test]
    async fn priming_pages_past_the_relays_fetch_limit_before_a_session_starts() {
        let (address, requests) = spawn_paging_frames_server().await;

        let mut channel = test_channel();
        channel.endpoints.frames_url = format!("http://{address}/frames");
        let cursor = channel.tail_cursor().await.expect("priming reaches the tail");

        // Stopping at the first page would start the session 172 frames short of
        // the tail, handing it the rest of a previous session's backlog — stale
        // authoritative state the protocol layer rejects, tearing the session
        // down on the very frames the priming exists to skip.
        assert_eq!(cursor, 300);
        // Three advancing pages plus the one that reports no movement.
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    /// ⚠️ A dropped frame must never read as a delivered one.
    ///
    /// This is where the mobile carrier has to DIVERGE from the plugin's, which
    /// it is otherwise a port of. The plugin posts authoritative state and
    /// republishes it, so swallowing a full mailbox costs it a refresh. The
    /// Companion posts USER COMMANDS, and `send_text`'s callers read `true` as
    /// proof of delivery: the native call action path reports success to
    /// Android telecom and marks the lease request delivered, and a queued
    /// Tauri command resolves `Ok(())` to the UI. Swallowing the drop would tell
    /// the operator their answer/hang-up landed while it was discarded, and the
    /// lease would then wait forever on a decision nobody was asked for.
    #[tokio::test]
    async fn a_full_mailbox_is_reported_rather_than_passed_off_as_delivered() {
        let address = spawn_status_only_server(429).await;

        let mut channel = test_channel();
        channel.endpoints.frames_url = format!("http://{address}/frames");

        assert!(
            !channel.send_text("{\"kind\":\"mobile_hello\"}").await,
            "a frame the relay refused to store must not report success"
        );
    }

    /// Always answers with one status and no body.
    async fn spawn_status_only_server(status: u16) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener");
        let address = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 2048];
                    let _ = socket.read(&mut buffer).await;
                    let response = format!(
                        "HTTP/1.1 {status} Status\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        address
    }

    /// A minimal HTTP/1.1 server for the priming test.
    ///
    /// Hand-rolled rather than pulled from a framework: this crate has no
    /// dev-dependency on an HTTP server, and adding one to assert four request
    /// cursors would be a heavier dependency than the assertion is worth.
    /// `lastSeq` advances 128 per page up to 300, mirroring the relay's fetch
    /// cap against a mailbox that holds more than one page.
    async fn spawn_paging_frames_server() -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener");
        let address = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 2048];
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    seen.fetch_add(1, Ordering::SeqCst);
                    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let since: u64 = request
                        .split("since=")
                        .nth(1)
                        .and_then(|tail| {
                            tail.split(|c: char| !c.is_ascii_digit())
                                .next()
                                .and_then(|digits| digits.parse().ok())
                        })
                        .unwrap_or(0);
                    let body = format!(
                        "{{\"frames\":[],\"lastSeq\":{}}}",
                        (since + 128).min(300)
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (address, requests)
    }

    fn test_channel() -> RelayChannel {
        RelayChannel {
            client: Client::builder().build().expect("test client builds"),
            endpoints: RelayEndpoints {
                challenge_url: "https://api.example.test/api/aokie-companion/relay/challenge"
                    .into(),
                frames_url: "https://api.example.test/api/aokie-companion/relay/frames".into(),
                stream_url: "https://api.example.test/api/aokie-companion/relay/stream".into(),
            },
            token: "aokie-adm-v2.token".into(),
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            inbound: VecDeque::new(),
            stream: None,
            parser: SseParser::default(),
            pending_bytes: Vec::new(),
            last_seq: 0,
            last_stream_read: Instant::now(),
            stream_opened_at: Instant::now(),
            reopen_not_before: Instant::now(),
            stream_failures: 0,
        }
    }
}
