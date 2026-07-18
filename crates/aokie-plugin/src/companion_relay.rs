//! FormLogic-hosted Companion relay carrier.
//!
//! An authenticated frame mailbox standing in for the v2 signalling
//! WebSocket: the plugin POSTs frames addressed to one Companion party and
//! reads frames addressed to itself off a Server-Sent Events stream. The
//! relay never interprets a frame — it stores and forwards opaque JSON — so
//! every protocol decision still happens in
//! [`crate::companion_gateway::GatewaySession`], exactly as it does over the
//! socket.
//!
//! Two properties of the hosted mailbox drive the shape of this module:
//!
//! * **The stream has a hard 300-second lifetime.** It ends cleanly with an
//!   `event: end` marker and the client resumes from its cursor. That is a
//!   routine re-open, NOT an outage: surfacing it as a disconnect would
//!   fail-closed every remote media route — i.e. drop a live call — every
//!   five minutes.
//! * **There is no gateway process in the middle.** A POST names a single
//!   destination party, so the carrier has to know which Companion a frame
//!   belongs to. Peer-directed frames carry `deviceId` and are routed to the
//!   party that device speaks from; the broadcast frames (snapshot, idle,
//!   assistance) go to every party currently talking to us.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use aokie_protocol::v2::EndpointChallengeFrame;
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::companion_gateway::{RelayEndpoints, WorkerError};

/// The challenge is a small document on an already-authenticated route.
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Frame POSTs are small and must not wedge the session loop.
const POST_TIMEOUT: Duration = Duration::from_secs(10);
/// Opening the stream is a plain request; only the BODY is long-lived, so the
/// handshake gets a bounded budget while the read itself never times out as a
/// whole request. Matches `ADMISSION_RPC_TIMEOUT` because the open happens
/// inside the session loop, and that is how long a re-open can hold up
/// snapshot publication (telephony runs on the radio thread and is unaffected).
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Comfortably beyond the relay's 15-second heartbeat comment cadence: past
/// this with no bytes at all the stream is dead rather than idle.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// Pause before re-opening a stream that failed, so a relay outage cannot turn
/// into a request loop. A clean end-of-lifetime re-opens immediately.
const STREAM_REOPEN_DELAY: Duration = Duration::from_secs(1);
/// Consecutive stream failures absorbed as "quiet" before the session is told.
/// Frames survive server-side for 120s and we resume from a cursor, so a short
/// gap costs nothing — while a torn-down session costs a live call.
const MAX_ABSORBED_STREAM_FAILURES: u32 = 3;
/// `AokieCompanionRelayService::MAX_FRAMES_PER_POST`.
const MAX_FRAMES_PER_POST: usize = 64;
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
/// Device→party routes are bounded so a chatty peer cannot grow the map.
const MAX_TRACKED_DEVICES: usize = 64;
/// Throttle for the "nobody is listening" note.
const UNDELIVERABLE_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Frames addressed to the desktop party carry `from`; the addressing member
/// the plugin reads back is `deviceId`. Deliberately permissive — this is a
/// routing peek, and the session remains the strict decoder.
#[derive(Deserialize)]
struct FrameRouting {
    #[serde(default, rename = "deviceId")]
    device_id: Option<String>,
}

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
                "[aokie-plugin][companion] stage=relay_stream_discarded detail=One SSE event exceeded the buffer ceiling"
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
    plugin_id: String,
    /// `mobile:<thumbprint>` for every roster member this endpoint approves.
    /// Anything else is not a peer of ours, whatever the relay says.
    approved_parties: HashSet<String>,
    /// This session's signed hello, delivered to each party on first contact:
    /// with no gateway in the middle nothing else would ever hand a Companion
    /// our endpoint proof.
    hello: Option<String>,
    greeted: HashSet<String>,
    routes: HashMap<String, String>,
    parties: Vec<String>,
    inbound: VecDeque<String>,
    stream: Option<Response>,
    parser: SseParser,
    last_seq: u64,
    last_stream_read: Instant,
    /// When the live stream opened, so the relay's immediate `retry:` /
    /// `: connected` preamble cannot masquerade as a working carrier.
    stream_opened_at: Instant,
    reopen_not_before: Instant,
    stream_failures: u32,
    undeliverable: Option<(Instant, u64)>,
}

impl RelayChannel {
    /// Fetch this session's endpoint challenge. The caller validates it and
    /// signs the hello through the shared
    /// [`crate::companion_gateway::endpoint_hello`], so the relay and the
    /// socket provably produce the same proof from the same document.
    pub(crate) async fn connect(
        endpoints: &RelayEndpoints,
        token: &str,
        app_id: &str,
        plugin_id: &str,
        approved_thumbprints: Vec<String>,
    ) -> Result<(Self, EndpointChallengeFrame), WorkerError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| {
                WorkerError::rebootstrap("Companion relay HTTP client could not be created")
            })?;
        let mut channel = Self {
            client,
            endpoints: endpoints.clone(),
            token: token.to_string(),
            app_id: app_id.to_string(),
            plugin_id: plugin_id.to_string(),
            approved_parties: approved_thumbprints
                .into_iter()
                .map(|thumbprint| format!("mobile:{thumbprint}"))
                .collect(),
            hello: None,
            greeted: HashSet::new(),
            routes: HashMap::new(),
            parties: Vec::new(),
            inbound: VecDeque::new(),
            stream: None,
            parser: SseParser::default(),
            last_seq: 0,
            last_stream_read: Instant::now(),
            stream_opened_at: Instant::now(),
            reopen_not_before: Instant::now(),
            stream_failures: 0,
            undeliverable: None,
        };
        let challenge = channel.fetch_challenge().await?;
        channel.last_seq = channel.tail_cursor().await?;
        eprintln!(
            "[aokie-plugin][companion] stage=relay_challenge transport=relay app={} plugin={} resume_from={}",
            channel.app_id, channel.plugin_id, channel.last_seq
        );
        Ok((channel, challenge))
    }

    /// Where a NEW session starts reading.
    ///
    /// The mailbox keeps frames for 120 seconds, so a fresh channel that began
    /// at zero would replay a previous session's traffic: stale leases and
    /// claim proposals the session rejects, which tears the session down, which
    /// re-reads the same backlog on the next attempt — a crash loop that only
    /// ends when the TTL does. Starting at the current tail is what the socket
    /// gateway does implicitly by having no backlog at all.
    ///
    /// A rotation must NOT re-prime: [`Self::adopt_routing_from`] restores the
    /// predecessor's cursor so nothing that arrived mid-handshake is skipped.
    ///
    /// The reported `lastSeq` is the tail of ONE page — the relay caps a fetch
    /// at 128 rows while an app mailbox holds up to 512 — so priming has to
    /// page forward until the cursor stops moving. Stopping at the first page
    /// would leave the remainder of the previous session's backlog in front of
    /// the new stream, which is exactly the replay this exists to prevent.
    async fn tail_cursor(&self) -> Result<u64, WorkerError> {
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
    async fn tail_cursor_page(&self, since: u64) -> Result<u64, WorkerError> {
        let mut url = Url::parse(&self.endpoints.frames_url)
            .map_err(|_| WorkerError::rebootstrap("Companion relay framesUrl is invalid"))?;
        url.query_pairs_mut()
            .append_pair("since", &since.to_string())
            .append_pair("wait", "0");
        let response = self
            .request(reqwest::Method::GET, url.as_str())?
            .timeout(POST_TIMEOUT)
            .send()
            .await
            .map_err(|error| relay_transport_error("frames", &error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(relay_status_error("frames", status));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|error| relay_transport_error("frames", &error))?;
        Ok(body
            .get("lastSeq")
            .and_then(Value::as_u64)
            .unwrap_or(since)
            .max(since))
    }

    /// Carry a rotating session's continuity onto its replacement channel.
    ///
    /// The endpoint authority is unchanged across an admission rotation (the
    /// session asserts it), so learned routes stay valid — and the cursor MUST
    /// come across, otherwise the replacement re-reads everything the
    /// predecessor already handed to the session.
    ///
    /// `greeted` is deliberately not inherited: a rotated session carries a new
    /// session nonce, so every party has to receive the new hello.
    pub(crate) fn adopt_routing_from(&mut self, previous: &mut Self) {
        self.last_seq = previous.last_seq;
        self.routes = std::mem::take(&mut previous.routes);
        self.parties = std::mem::take(&mut previous.parties);
        self.inbound = std::mem::take(&mut previous.inbound);
    }

    /// Hand the carrier the signed hello for this session.
    pub(crate) fn arm(&mut self, hello: String) {
        self.hello = Some(hello);
        self.greeted.clear();
    }

    async fn fetch_challenge(&self) -> Result<EndpointChallengeFrame, WorkerError> {
        let response = self
            .request(reqwest::Method::GET, &self.endpoints.challenge_url)?
            .timeout(CHALLENGE_TIMEOUT)
            .send()
            .await
            .map_err(|error| relay_transport_error("challenge", &error))?;
        let status = response.status();
        if !status.is_success() {
            return Err(relay_status_error("challenge", status));
        }
        let body = response
            .text()
            .await
            .map_err(|error| relay_transport_error("challenge", &error))?;
        serde_json::from_str(&body)
            .map_err(|_| WorkerError::reconnect("Companion endpoint challenge is malformed"))
    }

    pub(crate) async fn send_text(&mut self, encoded: &str) -> Result<(), WorkerError> {
        let targets = self.route_targets(encoded);
        if targets.is_empty() {
            self.note_undeliverable();
            return Ok(());
        }
        self.undeliverable = None;
        for party in targets {
            // Nothing else in the relay carries the endpoint proof, so a party
            // hears our hello before it hears anything else from us.
            let mut batch: Vec<&str> = Vec::with_capacity(2);
            let greet = match self.hello.as_deref() {
                Some(hello) if !self.greeted.contains(&party) => Some(hello),
                _ => None,
            };
            batch.extend(greet);
            batch.push(encoded);
            for chunk in batch.chunks(MAX_FRAMES_PER_POST) {
                self.post_frames(&party, chunk).await?;
            }
            self.greeted.insert(party);
        }
        Ok(())
    }

    pub(crate) async fn recv_text(&mut self, tick: Duration) -> Result<Option<String>, WorkerError> {
        if let Some(frame) = self.inbound.pop_front() {
            return Ok(Some(frame));
        }
        if self.stream.is_none() {
            let now = Instant::now();
            if now < self.reopen_not_before {
                // Hold the caller's cadence. Returning immediately would spin
                // the session loop at full speed for the whole reopen delay.
                tokio::time::sleep(tick.min(self.reopen_not_before - now)).await;
                return Ok(None);
            }
            if let Err(error) = self.open_stream().await {
                self.reopen_not_before = Instant::now() + STREAM_REOPEN_DELAY;
                return self.note_stream_failure(error).map(|()| None);
            }
        }
        let Some(stream) = self.stream.as_mut() else {
            return Ok(None);
        };
        match tokio::time::timeout(tick, stream.chunk()).await {
            Err(_) => {
                if self.last_stream_read.elapsed() < STREAM_IDLE_TIMEOUT {
                    return Ok(None);
                }
                self.restart_stream(STREAM_REOPEN_DELAY);
                self.note_stream_failure(WorkerError::reconnect(
                    "Companion relay stream went silent past its heartbeat window",
                ))
                .map(|()| None)
            }
            Ok(Ok(Some(bytes))) => {
                self.last_stream_read = Instant::now();
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let events = self.parser.push(&text);
                self.note_stream_progress(!events.is_empty());
                self.absorb(events);
                Ok(self.inbound.pop_front())
            }
            Ok(Ok(None)) => {
                // The relay signs off with `event: end`; a body that just stops
                // is abnormal, so resume from the cursor but count it.
                self.restart_stream(STREAM_REOPEN_DELAY);
                self.note_stream_failure(WorkerError::reconnect(
                    "Companion relay stream ended without a resume marker",
                ))
                .map(|()| None)
            }
            Ok(Err(error)) => {
                let failure = relay_transport_error("stream", &error);
                self.restart_stream(STREAM_REOPEN_DELAY);
                self.note_stream_failure(failure).map(|()| None)
            }
        }
    }

    /// Clear the failure streak only when a read actually proves the carrier
    /// works.
    ///
    /// A successful OPEN proves nothing, and neither does the first byte: the
    /// relay flushes `retry:` and `: connected` the instant a stream opens, so
    /// a carrier that greets and dies on every cycle would reset the streak
    /// forever and retry in silence behind a Connected status — exactly the
    /// state [`MAX_ABSORBED_STREAM_FAILURES`] exists to escape. A session event
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
    /// cursor, so a brief gap costs nothing — which is why a handful of faults
    /// are swallowed rather than fail-closing remote media. A stream that keeps
    /// failing is a different thing entirely and has to reach the session:
    /// retrying forever in silence would leave the status reading Connected
    /// while no signalling moves at all.
    fn note_stream_failure(&mut self, error: WorkerError) -> Result<(), WorkerError> {
        self.stream_failures = self.stream_failures.saturating_add(1);
        if self.stream_failures > MAX_ABSORBED_STREAM_FAILURES {
            return Err(error);
        }
        eprintln!(
            "[aokie-plugin][companion] stage=relay_stream_retry attempt={} detail={}",
            self.stream_failures, error.message
        );
        Ok(())
    }

    pub(crate) async fn close(self) {
        // Nothing to hand back: the mailbox holds no per-connection state, and
        // frames already posted stay readable until their TTL expires.
        drop(self);
    }

    fn absorb(&mut self, events: Vec<RelayStreamEvent>) {
        for event in events {
            match event {
                RelayStreamEvent::Frame { seq, from, frame } => {
                    self.last_seq = self.last_seq.max(seq);
                    if !self.approved_parties.contains(&from) {
                        eprintln!(
                            "[aokie-plugin][companion] stage=relay_frame_ignored detail=A frame arrived from a party outside the approved roster"
                        );
                        continue;
                    }
                    self.learn_route(&from, &frame);
                    self.inbound.push_back(frame);
                }
                RelayStreamEvent::End { seq } => {
                    if let Some(seq) = seq {
                        self.last_seq = self.last_seq.max(seq);
                    }
                    // The relay's hard lifetime, not an outage. Re-open at
                    // once: no fail-closed, no reconnect attempt, no phase
                    // change — the session never learns this happened.
                    self.restart_stream(Duration::ZERO);
                }
            }
        }
    }

    fn learn_route(&mut self, from: &str, frame: &str) {
        if !self.parties.iter().any(|party| party == from) {
            self.parties.push(from.to_string());
        }
        let Ok(routing) = serde_json::from_str::<FrameRouting>(frame) else {
            return;
        };
        let Some(device_id) = routing.device_id else {
            return;
        };
        // A device's party is bound on first sight and never re-pointed. The
        // `deviceId` here is SELF-ASSERTED by whichever Companion sent the
        // frame — the socket gateway routed by authenticated connection
        // identity and never took a destination from frame content, so
        // honouring a later claim would let one approved Companion redirect
        // another device's peer-directed frames (rtc_signal, lease_revoke,
        // claim_decision, end_caller_result) into its own mailbox. The session
        // already treats a device→peer binding as immutable; the carrier holds
        // the same line.
        match self.routes.get(&device_id) {
            Some(bound) if bound == from => {}
            Some(_) => {
                eprintln!(
                    "[aokie-plugin][companion] stage=relay_route_conflict detail=A Companion claimed a device already bound to another party and the claim was refused"
                );
            }
            None => {
                if self.routes.len() >= MAX_TRACKED_DEVICES {
                    return;
                }
                self.routes.insert(device_id, from.to_string());
            }
        }
    }

    /// Peer-directed frames name their device; everything else is authoritative
    /// state every listening Companion needs.
    fn route_targets(&self, encoded: &str) -> Vec<String> {
        let device_id = serde_json::from_str::<FrameRouting>(encoded)
            .ok()
            .and_then(|routing| routing.device_id);
        match device_id {
            // An unknown device is never broadcast: a peer-directed frame must
            // not reach a Companion it was not addressed to.
            Some(device_id) => self.routes.get(&device_id).cloned().into_iter().collect(),
            None => self.parties.clone(),
        }
    }

    fn note_undeliverable(&mut self) {
        // `None` — not a zero count — is what marks a fresh window: the log
        // resets the counter, so treating "count reaches one" as first-of-window
        // would re-fire on every single frame and defeat the interval.
        let Some((since, dropped)) = self.undeliverable else {
            eprintln!(
                "[aokie-plugin][companion] stage=relay_no_destination frames=1 detail=No Companion party has spoken on this relay session"
            );
            self.undeliverable = Some((Instant::now(), 0));
            return;
        };
        let dropped = dropped + 1;
        if since.elapsed() >= UNDELIVERABLE_LOG_INTERVAL {
            eprintln!(
                "[aokie-plugin][companion] stage=relay_no_destination frames={dropped} detail=No Companion party has spoken on this relay session"
            );
            self.undeliverable = Some((Instant::now(), 0));
        } else {
            self.undeliverable = Some((since, dropped));
        }
    }

    fn restart_stream(&mut self, delay: Duration) {
        self.stream = None;
        self.parser = SseParser::default();
        self.last_stream_read = Instant::now();
        self.reopen_not_before = Instant::now() + delay;
    }

    async fn open_stream(&mut self) -> Result<(), WorkerError> {
        let mut url = Url::parse(&self.endpoints.stream_url)
            .map_err(|_| WorkerError::rebootstrap("Companion relay streamUrl is invalid"))?;
        url.query_pairs_mut()
            .append_pair("since", &self.last_seq.to_string());
        let response = tokio::time::timeout(
            STREAM_OPEN_TIMEOUT,
            self.request(reqwest::Method::GET, url.as_str())?
                .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
                .send(),
        )
        .await
        .map_err(|_| WorkerError::reconnect("Companion relay stream did not open in time"))?
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

    async fn post_frames(&self, party: &str, frames: &[&str]) -> Result<(), WorkerError> {
        if frames.is_empty() {
            return Ok(());
        }
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
                        // because nothing is draining it. Losing a signalling
                        // frame is recoverable — the session republishes state
                        // — whereas failing the session would fail-closed a
                        // live call over a server-side queue depth.
                        eprintln!(
                            "[aokie-plugin][companion] stage=relay_backpressure frames={} detail=The relay mailbox is full and the frames were dropped",
                            frames.len()
                        );
                        return Ok(());
                    }
                    relay_status_error("frames", status)
                }
                Err(error) => relay_transport_error("frames", &error),
            };
            if attempt >= POST_ATTEMPTS || error.kind != crate::companion_gateway::WorkerErrorKind::Reconnect {
                return Err(error);
            }
            tokio::time::sleep(POST_RETRY_DELAY).await;
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, WorkerError> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| WorkerError::rebootstrap("Companion admission token is invalid"))?;
        authorization.set_sensitive(true);
        Ok(self
            .client
            .request(method, url)
            .header(AUTHORIZATION, authorization)
            .header(
                "x-aokie-app-id",
                HeaderValue::from_str(&self.app_id)
                    .map_err(|_| WorkerError::rebootstrap("Companion app identity is invalid"))?,
            )
            .header(
                "x-aokie-plugin-id",
                HeaderValue::from_str(&self.plugin_id).map_err(|_| {
                    WorkerError::rebootstrap("Companion plugin identity is invalid")
                })?,
            ))
    }
}

/// Assemble the POST body around already-serialised frames.
///
/// The frames are embedded verbatim rather than decoded and re-encoded: the
/// relay preserves member order end to end, so the document a Companion reads
/// is the document the session produced.
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

fn relay_transport_error(stage: &str, error: &reqwest::Error) -> WorkerError {
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
    WorkerError::reconnect(format!("Companion relay {stage} {detail}"))
}

fn relay_status_error(stage: &str, status: StatusCode) -> WorkerError {
    match status {
        // The admission behind the bearer is finished; the worker rotates.
        StatusCode::UNAUTHORIZED => WorkerError::reconnect(format!(
            "Companion relay {stage} rejected the admission ({status})"
        )),
        // An owner turned the service off, or the app is gone. Retrying at
        // socket cadence would achieve nothing.
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::SERVICE_UNAVAILABLE => {
            WorkerError::rebootstrap(format!(
                "Companion relay {stage} is unavailable for this app ({status})"
            ))
        }
        _ => WorkerError::reconnect(format!("Companion relay {stage} failed ({status})")),
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
        let encoded = frame_event(
            7,
            "mobile:abc",
            "{\"kind\":\"lease_granted\",\"deviceId\":\"device_a\"}",
        );
        let (head, tail) = encoded.split_at(encoded.len() / 2);

        // A chunk boundary inside an event yields nothing until the event ends.
        assert!(parser.push(head).is_empty());
        let events = parser.push(tail);

        let [RelayStreamEvent::Frame { seq, from, frame }] = events.as_slice() else {
            panic!("the split event decodes to exactly one frame, got {events:?}");
        };
        assert_eq!(*seq, 7);
        assert_eq!(from, "mobile:abc");
        // The frame is handed on SEMANTICALLY, not byte for byte: re-serialising
        // through serde_json::Value sorts members. That is safe in this
        // direction because every signature the protocol checks is recomputed
        // from parsed claims via signing_bytes(), never over transport bytes.
        // (Outbound is the opposite: relay_post_body embeds our own frames
        // verbatim — locked by post_body_embeds_frames_verbatim_and_quotes_the_party.)
        assert_eq!(
            serde_json::from_str::<Value>(frame).unwrap(),
            serde_json::json!({"kind": "lease_granted", "deviceId": "device_a"})
        );
    }

    #[test]
    fn heartbeat_comments_and_retry_hints_carry_no_session_frame() {
        let mut parser = SseParser::default();

        assert!(parser.push("retry: 2000\n\n: connected\n\n: keepalive\n\n").is_empty());

        // The stream stays usable: a real event after the comments still lands.
        let events = parser.push(&frame_event(3, "mobile:abc", "{\"kind\":\"rtc_signal\"}"));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn end_of_lifetime_is_a_resume_marker_rather_than_a_failure() {
        let mut parser = SseParser::default();

        let events = parser.push("id: 42\nevent: end\ndata: {}\n\n");

        // `End` exists precisely so the carrier can re-open silently: mapping
        // the relay's 300s lifetime onto a disconnect would fail-closed a live
        // call every five minutes.
        assert_eq!(events, vec![RelayStreamEvent::End { seq: Some(42) }]);
    }

    #[test]
    fn crlf_streams_and_multi_line_data_decode_identically() {
        let mut parser = SseParser::default();

        let events = parser.push("id: 9\r\nevent: frame\r\ndata: {\"seq\":9,\"from\":\"mobile:abc\",\r\ndata: \"frame\":{\"kind\":\"lease_renewed\"}}\r\n\r\n");

        assert_eq!(
            events,
            vec![RelayStreamEvent::Frame {
                seq: 9,
                from: "mobile:abc".into(),
                frame: "{\"kind\":\"lease_renewed\"}".into(),
            }]
        );
    }

    #[test]
    fn post_body_embeds_frames_verbatim_and_quotes_the_party() {
        let body = relay_post_body(
            "mobile:abc",
            &["{\"kind\":\"plugin_hello\"}", "{\"kind\":\"plugin_snapshot\"}"],
        );

        assert_eq!(
            body,
            "{\"to\":\"mobile:abc\",\"frames\":[{\"kind\":\"plugin_hello\"},{\"kind\":\"plugin_snapshot\"}]}"
        );
        let decoded: Value = serde_json::from_str(&body).expect("relay body is valid JSON");
        assert_eq!(decoded["frames"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn peer_directed_frames_never_reach_a_companion_they_were_not_addressed_to() {
        let mut channel = test_channel();
        channel.learn_route("mobile:abc", "{\"kind\":\"claim_proposal\",\"deviceId\":\"device_a\"}");
        channel.learn_route("mobile:def", "{\"kind\":\"claim_proposal\",\"deviceId\":\"device_b\"}");

        assert_eq!(
            channel.route_targets("{\"kind\":\"plugin_rtc_signal\",\"deviceId\":\"device_a\"}"),
            vec!["mobile:abc".to_string()]
        );
        // Broadcast state reaches every party that has spoken to us.
        assert_eq!(
            channel.route_targets("{\"kind\":\"plugin_snapshot\"}"),
            vec!["mobile:abc".to_string(), "mobile:def".to_string()]
        );
        // An unrecognised device is dropped rather than fanned out.
        assert!(channel
            .route_targets("{\"kind\":\"plugin_rtc_signal\",\"deviceId\":\"device_z\"}")
            .is_empty());
    }

    #[test]
    fn an_approved_companion_cannot_re_point_another_devices_route_at_itself() {
        let mut channel = test_channel();
        channel.learn_route(
            "mobile:abc",
            "{\"kind\":\"claim_proposal\",\"deviceId\":\"device_a\"}",
        );

        // `deviceId` is self-asserted frame content, so a second approved
        // Companion claiming a bound device must not capture its signalling.
        // The socket gateway routed by authenticated identity and could never
        // be steered this way.
        channel.learn_route(
            "mobile:def",
            "{\"kind\":\"claim_proposal\",\"deviceId\":\"device_a\"}",
        );

        assert_eq!(
            channel.route_targets("{\"kind\":\"plugin_rtc_signal\",\"deviceId\":\"device_a\"}"),
            vec!["mobile:abc".to_string()],
            "the established binding survives a conflicting claim"
        );
        // The claimant is still a party, so broadcast state still reaches it.
        assert_eq!(
            channel.route_targets("{\"kind\":\"plugin_snapshot\"}"),
            vec!["mobile:abc".to_string(), "mobile:def".to_string()]
        );
    }

    #[test]
    fn the_undeliverable_note_is_throttled_rather_than_logged_per_frame() {
        let mut channel = test_channel();

        channel.note_undeliverable();
        let (window, _) = channel.undeliverable.expect("the first drop opens a window");

        for _ in 0..5 {
            channel.note_undeliverable();
        }

        let (still, dropped) = channel.undeliverable.expect("the window is still open");
        // Resetting the count on every log would make each drop look like the
        // first one and print a line per frame.
        assert_eq!(still, window, "the window start is not reset while it runs");
        assert_eq!(dropped, 5, "drops accumulate instead of re-logging");
    }

    #[test]
    fn frames_from_outside_the_approved_roster_are_ignored() {
        let mut channel = test_channel();

        channel.absorb(vec![
            RelayStreamEvent::Frame {
                seq: 4,
                from: "mobile:intruder".into(),
                frame: "{\"kind\":\"claim_proposal\",\"deviceId\":\"device_x\"}".into(),
            },
            RelayStreamEvent::Frame {
                seq: 5,
                from: "mobile:abc".into(),
                frame: "{\"kind\":\"claim_proposal\",\"deviceId\":\"device_a\"}".into(),
            },
        ]);

        assert_eq!(channel.inbound.len(), 1);
        assert!(channel.routes.get("device_x").is_none());
        assert_eq!(channel.routes.get("device_a").map(String::as_str), Some("mobile:abc"));
        // The cursor still advances past a skipped row so it is never re-read.
        assert_eq!(channel.last_seq, 5);
    }

    #[test]
    fn a_broken_stream_is_absorbed_briefly_then_reported() {
        let mut channel = test_channel();

        // A short gap is quiet time: frames live 120s server-side and every
        // re-open resumes from the cursor, so failing the session here would
        // fail-closed remote media over nothing.
        for attempt in 1..=MAX_ABSORBED_STREAM_FAILURES {
            assert!(
                channel
                    .note_stream_failure(WorkerError::reconnect("relay stream broke"))
                    .is_ok(),
                "failure {attempt} should still be absorbed"
            );
        }

        // A carrier that never recovers must reach the session rather than
        // looping in silence behind a Connected status.
        assert!(channel
            .note_stream_failure(WorkerError::reconnect("relay stream broke"))
            .is_err());

        // Only a byte actually read off the wire clears the streak — a
        // successful open proves nothing about a stream that dies on read.
        channel.stream_failures = 0;
        assert!(channel
            .note_stream_failure(WorkerError::reconnect("relay stream broke"))
            .is_ok());
    }

    #[test]
    fn the_relays_open_preamble_cannot_pass_for_a_working_carrier() {
        let mut channel = test_channel();
        channel.stream_failures = MAX_ABSORBED_STREAM_FAILURES;

        // The relay flushes `retry:` and `: connected` the instant a stream
        // opens. A carrier that greets and dies therefore reads bytes on every
        // cycle: if that cleared the streak, the escalation could never fire
        // and the session would retry forever behind a Connected status.
        channel.stream_opened_at = Instant::now();
        channel.note_stream_progress(false);
        assert_eq!(channel.stream_failures, MAX_ABSORBED_STREAM_FAILURES);
        assert!(channel
            .note_stream_failure(WorkerError::reconnect("relay stream broke"))
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

    /// The one test that actually executes a relay HTTP request.
    #[tokio::test]
    async fn priming_pages_past_the_relays_fetch_limit_before_a_session_starts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let router = axum::Router::new().route(
            "/frames",
            axum::routing::get(
                move |axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>| {
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        let since: u64 =
                            query.get("since").and_then(|raw| raw.parse().ok()).unwrap_or(0);
                        // `lastSeq` is the tail of ONE page: the relay caps a
                        // fetch at 128 rows while an app mailbox holds up to
                        // 512, so a backlog of 300 takes three fetches to walk.
                        let last_seq = (since + 128).min(300);
                        axum::Json(serde_json::json!({ "frames": [], "lastSeq": last_seq }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let mut channel = test_channel();
        channel.endpoints.frames_url = format!("http://{address}/frames");
        let cursor = channel.tail_cursor().await.expect("priming reaches the tail");

        // Stopping at the first page would start the session 172 frames short
        // of the tail, handing it the rest of a previous session's backlog —
        // stale leases the session rejects, tearing itself down on the frames
        // the priming exists to skip.
        assert_eq!(cursor, 300);
        // Three advancing pages plus the one that reports no movement.
        assert_eq!(requests.load(Ordering::SeqCst), 4);
        server.abort();
    }

    #[test]
    fn a_complete_event_is_never_discarded_by_the_buffer_ceiling() {
        let mut parser = SseParser::default();

        // One oversized event that never terminates is a broken stream and is
        // dropped — but it must not take finished frames with it. Nothing
        // re-reads a discarded frame: the carrier resumes from `last_seq`, and
        // a frame parsed out of the buffer never reached it.
        let mut chunk = frame_event(11, "mobile:abc", "{\"kind\":\"lease_granted\"}");
        chunk.push_str("event: frame\ndata: ");
        chunk.push_str(&"x".repeat(MAX_STREAM_BUFFER_BYTES + 1));

        let events = parser.push(&chunk);

        assert_eq!(
            events,
            vec![RelayStreamEvent::Frame {
                seq: 11,
                from: "mobile:abc".into(),
                frame: "{\"kind\":\"lease_granted\"}".into(),
            }],
            "the finished frame survives the oversized one behind it"
        );
        // The runaway event itself is gone, so the stream stays usable.
        assert!(parser.buffer.is_empty());
    }

    #[test]
    fn an_admission_rotation_carries_the_cursor_and_routes_but_re_greets() {
        let mut previous = test_channel();
        previous.arm("{\"kind\":\"plugin_hello\",\"sessionNonce\":\"a\"}".into());
        previous.absorb(vec![RelayStreamEvent::Frame {
            seq: 118,
            from: "mobile:abc".into(),
            frame: "{\"kind\":\"lease_granted\",\"deviceId\":\"device_a\"}".into(),
        }]);
        previous.greeted.insert("mobile:abc".into());

        let mut next = test_channel();
        next.arm("{\"kind\":\"plugin_hello\",\"sessionNonce\":\"b\"}".into());
        next.adopt_routing_from(&mut previous);

        // Without the cursor the replacement re-reads everything still inside
        // the 120s mailbox TTL — replaying stale leases at the session, which
        // tears it down and fails-closed a live call.
        assert_eq!(next.last_seq, 118);
        assert_eq!(
            next.route_targets("{\"kind\":\"plugin_rtc_signal\",\"deviceId\":\"device_a\"}"),
            vec!["mobile:abc".to_string()]
        );
        // A frame the predecessor had read but not yet handed over is not lost.
        assert_eq!(next.inbound.len(), 1);
        // The rotated session has a new nonce, so the new hello must go out
        // again rather than riding the predecessor's greeting.
        assert!(!next.greeted.contains("mobile:abc"));
    }

    fn test_channel() -> RelayChannel {
        RelayChannel {
            client: Client::builder().build().expect("test client builds"),
            endpoints: RelayEndpoints {
                challenge_url: "https://api.example.test/api/aokie-companion/relay/challenge".into(),
                frames_url: "https://api.example.test/api/aokie-companion/relay/frames".into(),
                stream_url: "https://api.example.test/api/aokie-companion/relay/stream".into(),
            },
            token: "aokie-adm-v2.token".into(),
            app_id: "app_a".into(),
            plugin_id: "aokie".into(),
            approved_parties: HashSet::from(["mobile:abc".to_string(), "mobile:def".to_string()]),
            hello: None,
            greeted: HashSet::new(),
            routes: HashMap::new(),
            parties: Vec::new(),
            inbound: VecDeque::new(),
            stream: None,
            parser: SseParser::default(),
            last_seq: 0,
            last_stream_read: Instant::now(),
            stream_opened_at: Instant::now(),
            reopen_not_before: Instant::now(),
            stream_failures: 0,
            undeliverable: None,
        }
    }
}
