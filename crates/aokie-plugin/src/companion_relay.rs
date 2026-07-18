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
//!   belongs to. Peer-directed frames and projected snapshots carry
//!   `deviceId` and are routed to the party that device proved in its signed
//!   hello; non-sensitive session-wide notices such as idle may be broadcast.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aokie_protocol::v2::{EndpointChallengeFrame, Grant};
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::companion_gateway::{RelayEndpoints, TransportDelivery, WorkerError};

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
/// Admission currently defines fewer scopes than this. Keep a hard ceiling so
/// a malformed relay envelope cannot turn one queued frame into unbounded
/// authenticated metadata.
const MAX_AUTHENTICATED_GRANTS: usize = 16;
/// Throttle for the "nobody is listening" note.
const UNDELIVERABLE_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// Process-local identity for one relay carrier. A completed asynchronous
/// greeting refresh may only install into the exact channel that launched it.
static NEXT_RELAY_CHANNEL_ID: AtomicU64 = AtomicU64::new(1);

/// Frames addressed to the desktop party carry `from`; the addressing member
/// the plugin reads back is `deviceId`. Deliberately permissive — this is a
/// routing peek, and the session remains the strict decoder.
#[derive(Deserialize)]
struct FrameRouting {
    #[serde(default)]
    kind: Option<String>,
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
        /// Admission subject authenticated by FormLogic for this exact frame.
        /// Missing or malformed legacy metadata is retained as `None`, which
        /// the session treats as no device identity rather than trusting the
        /// inner frame's self-asserted `deviceId`.
        subject_id: Option<String>,
        grants: HashSet<Grant>,
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
            let subject_id = payload
                .get("subjectId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let grants = parse_authenticated_grants(&payload);
            // The stored document is re-serialised rather than passed through
            // verbatim; every signature the session checks is recomputed from
            // parsed claims, so member order carries no meaning.
            let frame = serde_json::to_string(payload.get("frame")?).ok()?;
            Some(RelayStreamEvent::Frame {
                seq,
                from,
                subject_id,
                grants,
                frame,
            })
        }
        _ => None,
    }
}

/// Decode the relay's server-authenticated admission scopes fail-closed.
///
/// The frame itself is still queued when metadata is malformed, but it carries
/// an EMPTY authority set. That lets the session tolerate peer traffic without
/// letting missing, unknown, duplicate, or oversized metadata become ambient
/// permission.
fn parse_authenticated_grants(payload: &Value) -> HashSet<Grant> {
    let Some(values) = payload.get("grants").and_then(Value::as_array) else {
        return HashSet::new();
    };
    if values.len() > MAX_AUTHENTICATED_GRANTS {
        return HashSet::new();
    }
    let mut grants = HashSet::with_capacity(values.len());
    for value in values {
        let Ok(grant) = serde_json::from_value::<Grant>(value.clone()) else {
            return HashSet::new();
        };
        if !grants.insert(grant) {
            return HashSet::new();
        }
    }
    grants
}

/// Identity of one relay cursor namespace.
///
/// Sequence numbers have meaning only inside the exact mailbox resources for
/// one app/plugin party. Admission rotation may reuse a cursor and learned
/// routes only when every normalized endpoint and both addressing identities
/// remain equal. Bearer tokens are intentionally absent: they rotate while
/// the mailbox stays the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelayCursorDomain {
    challenge_url: Url,
    frames_url: Url,
    stream_url: Url,
    app_id: String,
    plugin_id: String,
}

impl RelayCursorDomain {
    pub(crate) fn from_endpoints(
        endpoints: &RelayEndpoints,
        app_id: &str,
        plugin_id: &str,
    ) -> Option<Self> {
        Some(Self {
            challenge_url: Url::parse(&endpoints.challenge_url).ok()?,
            frames_url: Url::parse(&endpoints.frames_url).ok()?,
            stream_url: Url::parse(&endpoints.stream_url).ok()?,
            app_id: app_id.to_owned(),
            plugin_id: plugin_id.to_owned(),
        })
    }
}

pub(crate) struct RelayChannel {
    channel_id: u64,
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
    routes: HashMap<String, VerifiedRoute>,
    parties: Vec<String>,
    /// Each queued frame with the roster party that actually posted it.
    ///
    /// The socket gateway routed by authenticated connection identity, so the
    /// session never had to ask who a frame came from. Here the sender is the
    /// only thing distinguishing one approved Companion from another, and
    /// dropping it would let any roster member act as any other.
    inbound: VecDeque<(String, Option<String>, HashSet<Grant>, String)>,
    /// The party that posted the frame [`Self::recv_text`] last returned.
    last_inbound_party: Option<String>,
    /// The server-authenticated admission subject attached to that same frame.
    last_inbound_subject: Option<String>,
    /// The server-authenticated admission grants attached to that same frame.
    last_inbound_grants: HashSet<Grant>,
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

/// Clone-only input for one asynchronous per-party greeting refresh.
///
/// It deliberately owns no mutable [`RelayChannel`] state. Challenge latency
/// therefore never borrows or stalls the authority loop, and completion is
/// installed only through the launch channel's `channel_id` fence.
#[derive(Clone)]
pub(crate) struct RelayGreetingRequest {
    channel_id: u64,
    client: Client,
    challenge_url: String,
    token: String,
    app_id: String,
    plugin_id: String,
}

impl RelayGreetingRequest {
    pub(crate) fn channel_id(&self) -> u64 {
        self.channel_id
    }

    pub(crate) async fn fetch_challenge(&self) -> Result<EndpointChallengeFrame, WorkerError> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| WorkerError::rebootstrap("Companion admission token is invalid"))?;
        authorization.set_sensitive(true);
        let response = self
            .client
            .get(&self.challenge_url)
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
            )
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
}

#[derive(Clone)]
struct VerifiedRoute {
    party: String,
    /// Server-authenticated admission grants from the signed hello frame that
    /// installed or refreshed this route. Never populated from inner JSON.
    grants: HashSet<Grant>,
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
        Self::connect_inner(
            endpoints,
            token,
            app_id,
            plugin_id,
            approved_thumbprints,
            true,
        )
        .await
    }

    /// Prepare a relay that will immediately inherit an existing relay's
    /// cursor and routes during admission rotation.
    ///
    /// A normal new channel walks the mailbox tail before it starts, because
    /// reading from zero would replay an earlier session. A same-carrier
    /// replacement must not do that walk: [`Self::adopt_routing_from`] copies
    /// the exact live cursor after the endpoint challenge succeeds. Skipping
    /// the redundant walk both avoids missing frames that arrive mid-open and
    /// bounds the background rotation to the one challenge request.
    pub(crate) async fn connect_replacement(
        endpoints: &RelayEndpoints,
        token: &str,
        app_id: &str,
        plugin_id: &str,
        approved_thumbprints: Vec<String>,
    ) -> Result<(Self, EndpointChallengeFrame), WorkerError> {
        Self::connect_inner(
            endpoints,
            token,
            app_id,
            plugin_id,
            approved_thumbprints,
            false,
        )
        .await
    }

    async fn connect_inner(
        endpoints: &RelayEndpoints,
        token: &str,
        app_id: &str,
        plugin_id: &str,
        approved_thumbprints: Vec<String>,
        prime_tail: bool,
    ) -> Result<(Self, EndpointChallengeFrame), WorkerError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| {
                WorkerError::rebootstrap("Companion relay HTTP client could not be created")
            })?;
        let mut channel = Self {
            channel_id: NEXT_RELAY_CHANNEL_ID.fetch_add(1, Ordering::Relaxed),
            client,
            endpoints: endpoints.clone(),
            token: token.to_string(),
            app_id: app_id.to_string(),
            plugin_id: plugin_id.to_string(),
            approved_parties: approved_thumbprints
                .iter()
                .map(|thumbprint| crate::companion_gateway::relay_party(thumbprint))
                .collect(),
            hello: None,
            last_inbound_party: None,
            last_inbound_subject: None,
            last_inbound_grants: HashSet::new(),
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
        if prime_tail {
            channel.last_seq = channel.tail_cursor().await?;
        }
        eprintln!(
            "[aokie-plugin][companion] stage=relay_challenge transport=relay app={} plugin={} resume_from={} continuity={}",
            channel.app_id,
            channel.plugin_id,
            channel.last_seq,
            if prime_tail { "tail" } else { "predecessor" }
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
    /// A rotation inside the SAME [`RelayCursorDomain`] must not re-prime:
    /// [`Self::adopt_routing_from`] restores the predecessor's cursor so
    /// nothing that arrived mid-handshake is skipped. A different domain does
    /// prime normally and cannot adopt that unrelated numeric cursor.
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
    pub(crate) fn cursor_domain(&self) -> Option<RelayCursorDomain> {
        RelayCursorDomain::from_endpoints(&self.endpoints, &self.app_id, &self.plugin_id)
    }

    pub(crate) fn adopt_routing_from(&mut self, previous: &mut Self) -> bool {
        if self.cursor_domain().is_none() || self.cursor_domain() != previous.cursor_domain() {
            return false;
        }
        self.last_seq = previous.last_seq;
        self.routes = std::mem::take(&mut previous.routes);
        self.parties = std::mem::take(&mut previous.parties);
        self.inbound = std::mem::take(&mut previous.inbound);
        self.last_inbound_party = previous.last_inbound_party.take();
        self.last_inbound_subject = previous.last_inbound_subject.take();
        self.last_inbound_grants = std::mem::take(&mut previous.last_inbound_grants);
        true
    }

    /// Who posted the frame the session is handling right now.
    ///
    /// `None` before any frame has been read. The value is only meaningful
    /// immediately after [`Self::recv_text`] returned `Some`, which is exactly
    /// how the session uses it.
    pub(crate) fn last_inbound_party(&self) -> Option<&str> {
        self.last_inbound_party.as_deref()
    }

    /// Admission subject authenticated by FormLogic for the frame just popped.
    pub(crate) fn last_inbound_subject(&self) -> Option<&str> {
        self.last_inbound_subject.as_deref()
    }

    /// Admission grants authenticated by FormLogic for the frame just popped.
    pub(crate) fn last_inbound_grants(&self) -> &HashSet<Grant> {
        &self.last_inbound_grants
    }

    /// Hand the carrier the signed hello for this session.
    pub(crate) fn arm(&mut self, hello: String) {
        self.hello = Some(hello);
        self.greeted.clear();
    }

    /// Install a newly signed hello and owe it to one party again.
    ///
    /// [`Self::arm`] is the whole-session version, for a rotation that mints a
    /// new session nonce. This is the per-party one, for a Companion that
    /// re-introduced itself. The caller first fetches a fresh relay challenge
    /// and signs a fresh proof for the EXISTING logical session; only then are
    /// the cached hello and greeting book changed together. Replaying the
    /// original cached hello here is unsafe because endpoint proofs live for
    /// at most 30 seconds while a logical session can live much longer.
    pub(crate) fn install_regreeting(
        &mut self,
        expected_channel_id: u64,
        party: &str,
        hello: String,
    ) -> bool {
        if self.channel_id != expected_channel_id {
            return false;
        }
        self.hello = Some(hello);
        self.greeted.remove(party);
        true
    }

    pub(crate) async fn fetch_challenge(&self) -> Result<EndpointChallengeFrame, WorkerError> {
        self.regreeting_request().fetch_challenge().await
    }

    pub(crate) fn regreeting_request(&self) -> RelayGreetingRequest {
        RelayGreetingRequest {
            channel_id: self.channel_id,
            client: self.client.clone(),
            challenge_url: self.endpoints.challenge_url.clone(),
            token: self.token.clone(),
            app_id: self.app_id.clone(),
            plugin_id: self.plugin_id.clone(),
        }
    }

    pub(crate) async fn send_text(
        &mut self,
        encoded: &str,
    ) -> Result<TransportDelivery, WorkerError> {
        let targets = self.route_targets(encoded);
        if targets.is_empty() {
            self.note_undeliverable();
            return Ok(TransportDelivery::Dropped);
        }
        self.undeliverable = None;
        let mut delivery = TransportDelivery::Delivered;
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
            let mut party_delivered = true;
            for chunk in batch.chunks(MAX_FRAMES_PER_POST) {
                if self.post_frames(&party, chunk).await? == TransportDelivery::Dropped {
                    party_delivered = false;
                    delivery = TransportDelivery::Dropped;
                    break;
                }
            }
            if party_delivered {
                self.greeted.insert(party);
            }
        }
        Ok(delivery)
    }

    pub(crate) async fn recv_text(
        &mut self,
        tick: Duration,
    ) -> Result<Option<String>, WorkerError> {
        if let Some(frame) = self.take_inbound() {
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
                Ok(self.take_inbound())
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
                RelayStreamEvent::Frame {
                    seq,
                    from,
                    subject_id,
                    grants,
                    frame,
                } => {
                    self.last_seq = self.last_seq.max(seq);
                    if !self.approved_parties.contains(&from) {
                        eprintln!(
                            "[aokie-plugin][companion] stage=relay_frame_ignored detail=A frame arrived from a party outside the approved roster"
                        );
                        continue;
                    }
                    self.inbound.push_back((from, subject_id, grants, frame));
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

    /// Pop the next frame, remembering who posted it.
    fn take_inbound(&mut self) -> Option<String> {
        let (from, subject_id, grants, frame) = self.inbound.pop_front()?;
        self.last_inbound_party = Some(from);
        self.last_inbound_subject = subject_id;
        self.last_inbound_grants = grants;
        Some(frame)
    }

    /// Remember a roster party for authoritative broadcasts.
    ///
    /// This deliberately does NOT inspect `deviceId`. Every inbound frame is
    /// peer-controlled until the session verifies its signed `mobile_hello`.
    fn learn_party(&mut self, from: &str) {
        if !self.parties.iter().any(|party| party == from) {
            self.parties.push(from.to_string());
        }
    }

    /// Install the route proved by a verified mobile hello.
    ///
    /// Rebinding is intentional. The session calls this only after the
    /// endpoint signature, owner-approved roster membership, app/plugin
    /// addressing and sender party all agree. A restarted/re-approved device
    /// therefore repairs a stale route instead of losing grants to an older
    /// mailbox.
    pub(crate) fn authorize_route(
        &mut self,
        device_id: &str,
        party: &str,
        authenticated_grants: &HashSet<Grant>,
    ) {
        if !self.approved_parties.contains(party) {
            return;
        }
        self.learn_party(party);
        if self.routes.len() >= MAX_TRACKED_DEVICES && !self.routes.contains_key(device_id) {
            return;
        }
        self.routes.insert(
            device_id.to_string(),
            VerifiedRoute {
                party: party.to_string(),
                grants: authenticated_grants.clone(),
            },
        );
    }

    /// Apply grant narrowing observed on a later server-authenticated frame.
    /// Ordinary actions may remove authority immediately, but only a newly
    /// verified signed hello may broaden it again through `authorize_route`.
    pub(crate) fn narrow_route_grants(
        &mut self,
        device_id: &str,
        party: &str,
        authenticated_grants: &HashSet<Grant>,
    ) {
        let Some(route) = self.routes.get_mut(device_id) else {
            return;
        };
        if route.party != party {
            return;
        }
        route
            .grants
            .retain(|grant| authenticated_grants.contains(grant));
    }

    /// Peer-directed frames name their device. An untargeted snapshot is a
    /// projection bug and fails closed here rather than broadcasting its raw
    /// caller/caption/offer state; only non-sensitive session-wide notices are
    /// eligible for fan-out.
    fn route_targets(&self, encoded: &str) -> Vec<String> {
        let Ok(routing) = serde_json::from_str::<FrameRouting>(encoded) else {
            return Vec::new();
        };
        match routing.device_id {
            // An unknown device is never broadcast: a peer-directed frame must
            // not reach a Companion it was not addressed to.
            Some(device_id) => self
                .routes
                .get(&device_id)
                .map(|route| route.party.clone())
                .into_iter()
                .collect(),
            // Assistance can contain the caller's question and conversational
            // context. It is untargeted in the socket dialect, so the relay
            // performs the missing trusted projection using only grants that
            // FormLogic authenticated on each verified route.
            None if routing.kind.as_deref() == Some("assistance_request") => self
                .parties
                .iter()
                .filter(|party| {
                    self.routes.values().any(|route| {
                        route.party.as_str() == party.as_str()
                            && route.grants.contains(&Grant::StateRead)
                            && route.grants.contains(&Grant::AssistanceRead)
                    })
                })
                .cloned()
                .collect(),
            // The only truly session-wide, non-sensitive frame. Everything
            // else must be explicitly targeted or deliberately projected
            // above; unknown untargeted kinds fail closed.
            None if routing.kind.as_deref() == Some("plugin_idle") => self.parties.clone(),
            None => Vec::new(),
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

    async fn post_frames(
        &self,
        party: &str,
        frames: &[&str],
    ) -> Result<TransportDelivery, WorkerError> {
        if frames.is_empty() {
            return Ok(TransportDelivery::Delivered);
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
                Ok(response) if response.status().is_success() => {
                    return Ok(TransportDelivery::Delivered)
                }
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
                        return Ok(TransportDelivery::Dropped);
                    }
                    relay_status_error("frames", status)
                }
                Err(error) => relay_transport_error("frames", &error),
            };
            if attempt >= POST_ATTEMPTS
                || error.kind != crate::companion_gateway::WorkerErrorKind::Reconnect
            {
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

    fn test_grants() -> HashSet<Grant> {
        HashSet::from([Grant::StateRead, Grant::RtcSignal, Grant::Monitor])
    }

    fn frame_event(seq: u64, from: &str, frame: &str) -> String {
        format!("id: {seq}\nevent: frame\ndata: {{\"seq\":{seq},\"from\":\"{from}\",\"subjectId\":\"device_a\",\"grants\":[\"state_read\",\"rtc_signal\",\"monitor\"],\"frame\":{frame}}}\n\n")
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

        let [RelayStreamEvent::Frame {
            seq,
            from,
            subject_id,
            grants,
            frame,
        }] = events.as_slice()
        else {
            panic!("the split event decodes to exactly one frame, got {events:?}");
        };
        assert_eq!(*seq, 7);
        assert_eq!(from, "mobile:abc");
        assert_eq!(subject_id.as_deref(), Some("device_a"));
        assert_eq!(grants, &test_grants());
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
    fn malformed_authenticated_grants_fail_closed_to_empty_authority() {
        let malformed = [
            None,
            Some(serde_json::json!("state_read")),
            Some(serde_json::json!(["state_read", "unknown_scope"])),
            Some(serde_json::json!(["state_read", "state_read"])),
            Some(Value::Array(
                (0..=MAX_AUTHENTICATED_GRANTS)
                    .map(|_| serde_json::json!("state_read"))
                    .collect(),
            )),
        ];

        for (index, grants) in malformed.into_iter().enumerate() {
            let mut payload = serde_json::json!({
                "seq": index + 1,
                "from": "mobile:abc",
                "frame": {"kind": "mobile_hello"}
            });
            if let Some(grants) = grants {
                payload["grants"] = grants;
            }
            let encoded = format!("id: {}\nevent: frame\ndata: {}\n\n", index + 1, payload);
            let events = SseParser::default().push(&encoded);
            let [RelayStreamEvent::Frame { grants, .. }] = events.as_slice() else {
                panic!("malformed grant metadata still carries a harmless frame");
            };
            assert!(grants.is_empty(), "case {index} must fail closed");
        }
    }

    #[test]
    fn missing_or_non_string_authenticated_subject_carries_no_device_identity() {
        for (index, subject) in [
            None,
            Some(Value::Null),
            Some(serde_json::json!({"id": "device_a"})),
        ]
        .into_iter()
        .enumerate()
        {
            let mut payload = serde_json::json!({
                "seq": index + 1,
                "from": "mobile:abc",
                "grants": ["state_read"],
                "frame": {"kind": "mobile_hello"}
            });
            if let Some(subject) = subject {
                payload["subjectId"] = subject;
            }
            let encoded = format!("id: {}\nevent: frame\ndata: {}\n\n", index + 1, payload);
            let events = SseParser::default().push(&encoded);
            let [RelayStreamEvent::Frame { subject_id, .. }] = events.as_slice() else {
                panic!("legacy subject metadata still carries a harmless frame");
            };
            assert!(subject_id.is_none(), "case {index} must fail closed");
        }
    }

    #[test]
    fn queued_frames_keep_their_own_authenticated_party_and_grants() {
        let mut channel = test_channel();
        channel.absorb(vec![
            RelayStreamEvent::Frame {
                seq: 1,
                from: "mobile:abc".into(),
                subject_id: Some("device_a".into()),
                grants: HashSet::from([Grant::StateRead]),
                frame: "{\"kind\":\"mobile_hello\"}".into(),
            },
            RelayStreamEvent::Frame {
                seq: 2,
                from: "mobile:def".into(),
                subject_id: Some("device_b".into()),
                grants: HashSet::from([Grant::StateRead, Grant::RtcSignal, Grant::Takeover]),
                frame: "{\"kind\":\"lease_request\"}".into(),
            },
        ]);

        assert!(channel.take_inbound().is_some());
        assert_eq!(channel.last_inbound_party(), Some("mobile:abc"));
        assert_eq!(channel.last_inbound_subject(), Some("device_a"));
        assert_eq!(
            channel.last_inbound_grants(),
            &HashSet::from([Grant::StateRead])
        );
        assert!(channel.take_inbound().is_some());
        assert_eq!(channel.last_inbound_party(), Some("mobile:def"));
        assert_eq!(channel.last_inbound_subject(), Some("device_b"));
        assert_eq!(
            channel.last_inbound_grants(),
            &HashSet::from([Grant::StateRead, Grant::RtcSignal, Grant::Takeover])
        );
    }

    #[test]
    fn heartbeat_comments_and_retry_hints_carry_no_session_frame() {
        let mut parser = SseParser::default();

        assert!(parser
            .push("retry: 2000\n\n: connected\n\n: keepalive\n\n")
            .is_empty());

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

        let events = parser.push("id: 9\r\nevent: frame\r\ndata: {\"seq\":9,\"from\":\"mobile:abc\",\"subjectId\":\"device_a\",\"grants\":[\"state_read\",\"rtc_signal\",\"monitor\"],\r\ndata: \"frame\":{\"kind\":\"lease_renewed\"}}\r\n\r\n");

        assert_eq!(
            events,
            vec![RelayStreamEvent::Frame {
                seq: 9,
                from: "mobile:abc".into(),
                subject_id: Some("device_a".into()),
                grants: test_grants(),
                frame: "{\"kind\":\"lease_renewed\"}".into(),
            }]
        );
    }

    #[test]
    fn post_body_embeds_frames_verbatim_and_quotes_the_party() {
        let body = relay_post_body(
            "mobile:abc",
            &[
                "{\"kind\":\"plugin_hello\"}",
                "{\"kind\":\"plugin_snapshot\"}",
            ],
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
        channel.learn_party("mobile:abc");
        channel.learn_party("mobile:def");
        channel.authorize_route("device_a", "mobile:abc", &test_grants());
        channel.authorize_route("device_b", "mobile:def", &test_grants());

        assert_eq!(
            channel.route_targets("{\"kind\":\"plugin_rtc_signal\",\"deviceId\":\"device_a\"}"),
            vec!["mobile:abc".to_string()]
        );
        // A projected snapshot follows its verified device route. A raw
        // snapshot is a security-boundary regression and fails closed instead
        // of exposing one device's caller/captions/offers to another.
        assert_eq!(
            channel.route_targets("{\"kind\":\"plugin_snapshot\",\"deviceId\":\"device_a\"}"),
            vec!["mobile:abc".to_string()]
        );
        assert!(channel
            .route_targets("{\"kind\":\"plugin_snapshot\"}")
            .is_empty());
        // The idle notice carries no call data and remains session-wide.
        assert_eq!(
            channel.route_targets("{\"kind\":\"plugin_idle\"}"),
            vec!["mobile:abc".to_string(), "mobile:def".to_string()]
        );
        // An unrecognised device is dropped rather than fanned out.
        assert!(channel
            .route_targets("{\"kind\":\"plugin_rtc_signal\",\"deviceId\":\"device_z\"}")
            .is_empty());
        assert!(
            channel.route_targets("not json").is_empty(),
            "malformed internal output is never broadcast"
        );
    }

    #[test]
    fn untargeted_assistance_reaches_only_verified_assistance_read_routes() {
        let mut channel = test_channel();
        let allowed = HashSet::from([Grant::StateRead, Grant::AssistanceRead]);
        let denied = HashSet::from([Grant::StateRead]);
        let assistance_without_state = HashSet::from([Grant::AssistanceRead]);
        channel.authorize_route("device_a", "mobile:abc", &allowed);
        channel.learn_party("mobile:def");
        channel.authorize_route("device_c", "mobile:ghi", &assistance_without_state);
        let assistance =
            "{\"kind\":\"assistance_request\",\"question\":\"private\",\"context\":\"caller context\"}";
        assert_eq!(
            channel.route_targets(assistance),
            vec!["mobile:abc".to_string()],
            "a party needs both StateRead and AssistanceRead before receiving caller context"
        );
        channel.authorize_route("device_b", "mobile:def", &denied);

        assert_eq!(
            channel.route_targets(assistance),
            vec!["mobile:abc".to_string()],
            "a verified route without AssistanceRead still receives no caller context"
        );
        channel.narrow_route_grants("device_a", "mobile:abc", &HashSet::from([Grant::StateRead]));
        assert!(
            channel.route_targets(assistance).is_empty(),
            "a later authenticated frame that omits AssistanceRead narrows the verified route immediately"
        );
        assert!(
            channel
                .route_targets(
                    "{\"kind\":\"future_sensitive_notice\",\"context\":\"must not fan out\"}"
                )
                .is_empty(),
            "unknown untargeted kinds fail closed instead of becoming ambient broadcasts"
        );
    }

    #[test]
    fn a_pre_hello_squatter_cannot_capture_a_grant_and_verified_hello_repairs_the_route() {
        let mut channel = test_channel();
        channel.absorb(vec![RelayStreamEvent::Frame {
            seq: 1,
            from: "mobile:abc".into(),
            subject_id: Some("device_a".into()),
            grants: test_grants(),
            // Arbitrary pre-hello traffic may name another device, but that
            // field is self-asserted and therefore creates no targeted route.
            frame: "{\"kind\":\"lease_request\",\"deviceId\":\"device_a\"}".into(),
        }]);

        let grant = "{\"kind\":\"plugin_lease_status\",\"deviceId\":\"device_a\"}";
        assert!(
            channel.route_targets(grant).is_empty(),
            "unverified traffic cannot capture a targeted grant"
        );

        // GatewaySession calls this only after device_a's signed hello proves
        // that mobile:def is its endpoint party. The verified route wins even
        // if untrusted traffic tried to squat first.
        channel.authorize_route("device_a", "mobile:def", &test_grants());

        assert_eq!(
            channel.route_targets(grant),
            vec!["mobile:def".to_string()],
            "the verified hello owns the grant route"
        );
        // Garbage from a roster member does not subscribe it to raw snapshots.
        // Projected state follows only the route installed by the verified
        // hello; raw state is never broadcast at all.
        assert_eq!(
            channel.route_targets("{\"kind\":\"plugin_snapshot\",\"deviceId\":\"device_a\"}"),
            vec!["mobile:def".to_string()]
        );
        assert!(channel
            .route_targets("{\"kind\":\"plugin_snapshot\"}")
            .is_empty());
    }

    #[test]
    fn the_undeliverable_note_is_throttled_rather_than_logged_per_frame() {
        let mut channel = test_channel();

        channel.note_undeliverable();
        let (window, _) = channel
            .undeliverable
            .expect("the first drop opens a window");

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
                subject_id: Some("device_x".into()),
                grants: test_grants(),
                frame: "{\"kind\":\"claim_proposal\",\"deviceId\":\"device_x\"}".into(),
            },
            RelayStreamEvent::Frame {
                seq: 5,
                from: "mobile:abc".into(),
                subject_id: Some("device_a".into()),
                grants: test_grants(),
                frame: "{\"kind\":\"claim_proposal\",\"deviceId\":\"device_a\"}".into(),
            },
        ]);

        assert_eq!(channel.inbound.len(), 1);
        assert!(channel.routes.get("device_x").is_none());
        assert!(channel.routes.get("device_a").is_none());
        assert!(
            channel.parties.is_empty(),
            "an approved sender is not a broadcast subscriber before verified hello"
        );
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
                move |axum::extract::Query(query): axum::extract::Query<
                    HashMap<String, String>,
                >| {
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        let since: u64 = query
                            .get("since")
                            .and_then(|raw| raw.parse().ok())
                            .unwrap_or(0);
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
        let cursor = channel
            .tail_cursor()
            .await
            .expect("priming reaches the tail");

        // Stopping at the first page would start the session 172 frames short
        // of the tail, handing it the rest of a previous session's backlog —
        // stale leases the session rejects, tearing itself down on the frames
        // the priming exists to skip.
        assert_eq!(cursor, 300);
        // Three advancing pages plus the one that reports no movement.
        assert_eq!(requests.load(Ordering::SeqCst), 4);
        server.abort();
    }

    #[tokio::test]
    async fn terminal_backpressure_is_a_nonfatal_drop_not_a_delivery() {
        let router = axum::Router::new().route(
            "/frames",
            axum::routing::post(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
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
        channel.arm("{\"kind\":\"plugin_hello\"}".into());
        channel.authorize_route("device_a", "mobile:abc", &test_grants());
        let delivery = channel
            .send_text("{\"kind\":\"plugin_lease_status\",\"deviceId\":\"device_a\"}")
            .await
            .expect("backpressure is nonfatal to the live session");

        assert_eq!(delivery, TransportDelivery::Dropped);
        assert!(
            !channel.greeted.contains("mobile:abc"),
            "a dropped hello/grant batch must be retried as a greeting later"
        );
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
                subject_id: Some("device_a".into()),
                grants: test_grants(),
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
            subject_id: Some("device_a".into()),
            grants: test_grants(),
            frame: "{\"kind\":\"lease_granted\",\"deviceId\":\"device_a\"}".into(),
        }]);
        previous.authorize_route("device_a", "mobile:abc", &test_grants());
        previous.greeted.insert("mobile:abc".into());

        let mut next = test_channel();
        next.arm("{\"kind\":\"plugin_hello\",\"sessionNonce\":\"b\"}".into());
        assert!(next.adopt_routing_from(&mut previous));

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

    #[test]
    fn a_cursor_is_never_adopted_across_relay_domain_or_party_changes() {
        fn primed_previous() -> RelayChannel {
            let mut previous = test_channel();
            previous.absorb(vec![RelayStreamEvent::Frame {
                seq: 118,
                from: "mobile:abc".into(),
                subject_id: Some("device_a".into()),
                grants: test_grants(),
                frame: "{\"kind\":\"lease_heartbeat\"}".into(),
            }]);
            previous.authorize_route("device_a", "mobile:abc", &test_grants());
            previous
        }

        let mutations: [fn(&mut RelayChannel); 3] = [
            |channel| {
                channel.endpoints.challenge_url =
                    "https://other.example.test/api/aokie-companion/relay/challenge".into();
                channel.endpoints.frames_url =
                    "https://other.example.test/api/aokie-companion/relay/frames".into();
                channel.endpoints.stream_url =
                    "https://other.example.test/api/aokie-companion/relay/stream".into();
            },
            |channel| channel.app_id = "app_b".into(),
            |channel| channel.plugin_id = "another_plugin".into(),
        ];
        for mutate in mutations {
            let mut previous = primed_previous();
            let mut next = test_channel();
            next.last_seq = 7;
            mutate(&mut next);

            assert!(!next.adopt_routing_from(&mut previous));
            assert_eq!(next.last_seq, 7, "the new mailbox keeps its own cursor");
            assert!(next.routes.is_empty(), "old routes cannot cross domains");
            assert_eq!(previous.last_seq, 118, "the predecessor is untouched");
            assert!(previous.routes.contains_key("device_a"));
            assert_eq!(previous.inbound.len(), 1);
        }

        // URL parsing canonicalizes host case and the default HTTPS port, so
        // spelling-only differences do not manufacture a false domain change.
        let mut previous = primed_previous();
        let mut normalized_equivalent = test_channel();
        normalized_equivalent.endpoints.challenge_url =
            "https://API.EXAMPLE.TEST:443/api/aokie-companion/relay/challenge".into();
        normalized_equivalent.endpoints.frames_url =
            "https://API.EXAMPLE.TEST:443/api/aokie-companion/relay/frames".into();
        normalized_equivalent.endpoints.stream_url =
            "https://API.EXAMPLE.TEST:443/api/aokie-companion/relay/stream".into();
        assert!(normalized_equivalent.adopt_routing_from(&mut previous));
        assert_eq!(normalized_equivalent.last_seq, 118);
    }

    #[test]
    fn a_re_introduced_companion_is_owed_the_hello_again_without_disturbing_the_others() {
        let mut channel = test_channel();
        channel.arm("{\"kind\":\"plugin_hello\",\"sessionNonce\":\"a\"}".into());
        channel.greeted.insert("mobile:abc".into());
        channel.greeted.insert("mobile:def".into());

        assert!(channel.install_regreeting(
            channel.channel_id,
            "mobile:abc",
            "{\"kind\":\"plugin_hello\",\"sessionNonce\":\"a\",\"proof\":\"fresh\"}".into(),
        ));

        // The party that re-introduced itself hears the hello again: a
        // restarted Companion holds no memory of the first one, and it DROPS
        // authoritative state from a peer it cannot verify — so without this it
        // would receive every frame and use none of them.
        assert!(!channel.greeted.contains("mobile:abc"));
        // ...and nobody else is disturbed. Re-greeting the whole session is
        // `arm`'s job, on a rotation that actually changed the session nonce.
        assert!(channel.greeted.contains("mobile:def"));
        assert_eq!(
            channel.hello.as_deref(),
            Some("{\"kind\":\"plugin_hello\",\"sessionNonce\":\"a\",\"proof\":\"fresh\"}")
        );
    }

    #[test]
    fn a_late_regreeting_cannot_install_into_a_rotated_channel() {
        let previous = test_channel();
        let mut replacement = test_channel();
        replacement.arm("{\"kind\":\"plugin_hello\",\"sessionNonce\":\"new\"}".into());
        replacement.greeted.insert("mobile:abc".into());

        assert!(!replacement.install_regreeting(
            previous.channel_id,
            "mobile:abc",
            "{\"kind\":\"plugin_hello\",\"sessionNonce\":\"old\"}".into(),
        ));
        assert_eq!(
            replacement.hello.as_deref(),
            Some("{\"kind\":\"plugin_hello\",\"sessionNonce\":\"new\"}")
        );
        assert!(
            replacement.greeted.contains("mobile:abc"),
            "late work must not disturb the replacement channel's greeting book"
        );
    }

    #[test]
    fn the_party_the_session_retires_is_the_one_the_carrier_approved() {
        // The greeting book, the approved set and the session's re-greet signal
        // must agree on how a party is spelled, or the retirement silently
        // targets a party that does not exist and the deadlock is unchanged.
        let channel = test_channel();
        let party = crate::companion_gateway::relay_party("abc");

        assert!(channel.approved_parties.contains(&party));
        assert_eq!(party, "mobile:abc");
    }

    fn test_channel() -> RelayChannel {
        RelayChannel {
            channel_id: NEXT_RELAY_CHANNEL_ID.fetch_add(1, Ordering::Relaxed),
            client: Client::builder().build().expect("test client builds"),
            endpoints: RelayEndpoints {
                challenge_url: "https://api.example.test/api/aokie-companion/relay/challenge"
                    .into(),
                frames_url: "https://api.example.test/api/aokie-companion/relay/frames".into(),
                stream_url: "https://api.example.test/api/aokie-companion/relay/stream".into(),
            },
            token: "aokie-adm-v2.token".into(),
            app_id: "app_a".into(),
            plugin_id: "aokie".into(),
            approved_parties: HashSet::from(["mobile:abc".to_string(), "mobile:def".to_string()]),
            hello: None,
            last_inbound_party: None,
            last_inbound_subject: None,
            last_inbound_grants: HashSet::new(),
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
