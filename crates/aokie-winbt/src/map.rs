//! MAP (Message Access Profile) over the WinRT RFCOMM channel — SMS
//! send + 45 s inbox poll for the native Windows backend.
//!
//! Phase 2 of `docs/NATIVE_BLUETOOTH_TRANSPORT_PLAN.md`: "SMS via MAP
//! MAS over WinRT RFCOMM — reuse `obex.rs`, `map_mas.rs`, `bmessage.rs`,
//! `map_listing.rs`" and "PollInbox fallback keeps replies working from
//! day one".
//!
//! REUSED VERBATIM from `aokie_radio` (the wire formats are the hard
//! won part — Pixel/Bluedroid quirks are baked into them):
//!   - `obex.rs`        — packet/header codec (via `rfcomm::read_obex_packet`)
//!   - `map_mas.rs`     — `MasMceSession`, the per-op OBEX state machine
//!                        (CONNECT → SETPATH telecom/msg/<folder> →
//!                        GET/PUT), including its SRM (Single Response
//!                        Mode) latch and keep-alive pooling
//!   - `map_listing.rs` — `x-bt/MAP-msg-listing` XML parser
//!   - `bmessage.rs`    — bMessage build/parse (SMS + MMS envelopes)
//!
//! REWRITTEN (deliberately): the dongle's orchestration (`map_runtime.rs`)
//! is event-driven over its own L2CAP/RFCOMM mux and cannot run on a
//! blocking socket. `MapLoop` drives the SAME session machine with a
//! compact synchronous loop (write request → read one OBEX response →
//! feed it back), mirroring the runtime's session semantics: pooled
//! keep-alive MAS channel, SetNotificationRegistration on open, 45 s
//! poll cadence, 20 s post-send quiet window, first-seed catch-up rules.
//! Runs entirely on the crate's worker thread, where blocking reads are
//! acceptable for v1 (see rfcomm.rs's blocking-by-design note).
//!
//! MNS (phone-pushed notifications) is NOT hosted here — that needs an
//! `RfcommServiceProvider` + custom SDP record, a later phase (plan §5).
//! The 45 s poll is the inbound channel, exactly like the dongle's
//! fallback on outbound sessions. We still send
//! SetNotificationRegistration(1) once per channel for diagnostic parity
//! (and because some phones gate MAS features on it); its ack emits
//! `MapNotificationsSubscribed` exactly like the dongle.

#![allow(dead_code)] // Phase 2 — worker wiring lands with the calls/audio engines.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aokie_bluetooth::aokie_radio::bmessage;
use aokie_bluetooth::aokie_radio::map_listing::{self, MessageEntry};
use aokie_bluetooth::aokie_radio::map_mas::{
    self, Folder, MasMceSession, MasState, Operation, OperationOutput,
};
use aokie_bluetooth::aokie_radio::obex::FixedPayload;
use aokie_dongle::bluetooth::{BluetoothEvent, SmsReceivedPayload};

use crate::pbap::PbapFetch;
use crate::rfcomm::{read_obex_packet, BtChannel, MAX_OBEX_ROUND_TRIPS};
use crate::runtime::NativeShared;

/// MAS instance id we always talk to. Mirrors
/// `aokie_radio::runtime::MAS_INSTANCE_ID_FOR_SMS`: instance 0 is SMS on
/// every Pixel / iPhone tested; non-zero is reserved for email/IM.
const MAS_INSTANCE_ID_FOR_SMS: u8 = 0;

/// Inbox poll cadence — mirrors the dongle's INBOX_POLL_INTERVAL (45 s
/// balances catch-up latency against wasted MAS traffic on quiet
/// threads).
const INBOX_POLL_INTERVAL: Duration = Duration::from_secs(45);

/// Top-N handles per poll (listing returns newest-first). Mirrors the
/// dongle's INBOX_POLL_MAX_LIST: small enough that the whole listing
/// body fits one OBEX packet, so the Pixel MAS never flips into its
/// flaky SRM streaming mode for the poll.
const INBOX_POLL_MAX_LIST: u16 = 5;

/// How long after a successful PushMessage the poll stays quiet.
/// Mirrors the dongle's POLL_SKIP_AFTER_REPLY: Pixel's MAS goes silent
/// when a SETPATH lands within tens of seconds of an outbox PUT.
const POLL_SKIP_AFTER_REPLY: Duration = Duration::from_secs(20);

/// First-seed catch-up window: an UNREAD top-of-inbox message younger
/// than this was very likely sent while our inbound path was down
/// (mirrors the dongle's one-hour rule).
const CATCHUP_RECENT_HOURS: i64 = 1;

/// Steady-state window: a newly-seen handle is only fetched if unread
/// and younger than this — anything older is history, and "a surprise
/// late reply is worse than silence".
const NEW_MSG_RECENT_HOURS: i64 = 24;

/// SMS send/receive + inbox polling for the native backend. One per
/// worker; owns the pooled MAS channel and the PBAP one-shot fetch.
pub(crate) struct MapLoop {
    event_tx: Sender<BluetoothEvent>,
    shared: Arc<NativeShared>,
    /// Pooled keep-alive MAS connection; dropped on any op failure and
    /// lazily re-opened by the next poll/send.
    mas: Option<MasConnection>,
    pbap: PbapFetch,
    /// Inbox handles already processed. Survives channel/connection
    /// drops (handles are persistent per phone); reset only when the
    /// PHONE changes.
    seen_handles: HashSet<String>,
    inbox_poll_seeded: bool,
    /// Polls started while still unseeded — mirrors the dongle's
    /// `seed_attempts`: >= 2 means an earlier seed attempt died before
    /// its listing arrived, so the retry fetches the top entry outright.
    seed_attempts: usize,
    last_poll_at: Option<Instant>,
    last_send_at: Option<Instant>,
    notifications_subscribed: bool,
    /// Subscribe failed once on this connection — don't pay a second
    /// doomed OBEX CONNECT for it on every later channel open.
    subscribe_failed: bool,
    /// Last poll failure text already surfaced as an `Error` event —
    /// dedupes the event to one per failure streak.
    last_poll_error: Option<String>,
    /// Consecutive poll failures — the Error event fires only from the
    /// third, so a flapping idle link doesn't pin the health banner.
    poll_fail_streak: u32,
    /// Phone the seen-set + subscription flags belong to.
    state_phone: Option<u64>,
}

impl MapLoop {
    pub(crate) fn new(event_tx: Sender<BluetoothEvent>, shared: Arc<NativeShared>) -> Self {
        Self {
            event_tx,
            shared,
            mas: None,
            pbap: PbapFetch::new(),
            seen_handles: HashSet::new(),
            inbox_poll_seeded: false,
            seed_attempts: 0,
            last_poll_at: None,
            last_send_at: None,
            notifications_subscribed: false,
            subscribe_failed: false,
            last_poll_error: None,
            poll_fail_streak: 0,
            state_phone: None,
        }
    }

    /// Blocking: send one SMS now. Emits SmsSent / SmsSendFailed events.
    pub(crate) fn send_sms(
        &mut self,
        message_id: &str,
        recipient: &str,
        body: &str,
        msg_type: Option<String>,
    ) -> Result<(), String> {
        let (_connected, phone) = connection_snapshot(&self.shared);
        // Gate on the KNOWN phone, not the HFP line: Windows drops the idle
        // link and every op re-forges it on demand (live-proven 2026-07-19).
        let result = if phone != 0 {
            let bmessage = build_outgoing_bmessage(msg_type.as_deref(), recipient, body);
            self.with_mas(
                phone,
                Operation::PushMessage {
                    folder: Folder::Outbox,
                    bmessage,
                    charset: map_mas::CHARSET_UTF8,
                },
            )
            .map(|_| ())
        } else {
            Err("phone not connected".to_string())
        };
        match result {
            Ok(()) => {
                // Stamp the quiet window BEFORE the event so a poll that
                // lands between ack and event still defers (POLL_SKIP_AFTER_REPLY).
                self.last_send_at = Some(Instant::now());
                let _ = self.event_tx.send(BluetoothEvent::SmsSent {
                    message_id: message_id.to_string(),
                    recipient_phone: recipient.to_string(),
                });
                Ok(())
            }
            Err(reason) => {
                let _ = self.event_tx.send(BluetoothEvent::SmsSendFailed {
                    message_id: message_id.to_string(),
                    recipient_phone: recipient.to_string(),
                    reason: reason.clone(),
                });
                Err(reason)
            }
        }
    }

    /// Called ~every worker pass (~50-100 ms). Manages the ~45 s inbox
    /// poll cadence while connected. Cheap when idle: the cadence gate
    /// returns before any I/O.
    pub(crate) fn tick(&mut self) {
        let (_connected, phone) = connection_snapshot(&self.shared);
        if phone == 0 {
            // No phone known: drop the channel and per-connection flags. The
            // seen-set and seed state SURVIVE — handles are persistent
            // per phone and re-emitting already-seen messages after a
            // reconnect would resurrect the bot's replies to them.
            self.drop_connection_state();
            self.state_phone = None;
            // PBAP has its own disconnect reset (it refetches per connect).
            self.pbap.tick(&self.event_tx, &self.shared);
            return;
        }
        if self.state_phone != Some(phone) {
            self.drop_connection_state();
            self.seen_handles.clear();
            self.inbox_poll_seeded = false;
            self.seed_attempts = 0;
            self.state_phone = Some(phone);
        }
        // Phonebook rides the same cadence gate as the poll setup (it is
        // a one-shot per connection and gates itself internally).
        self.pbap.tick(&self.event_tx, &self.shared);

        let now = Instant::now();
        if !poll_due(self.last_poll_at, self.last_send_at, now) {
            return;
        }
        self.last_poll_at = Some(now);
        if !self.inbox_poll_seeded {
            self.seed_attempts = self.seed_attempts.saturating_add(1);
        }
        match self.poll_inbox(phone) {
            Ok(()) => {
                self.last_poll_error = None;
                self.poll_fail_streak = 0;
            }
            Err(e) => {
                // Dampen flapping-link noise: a native-mode link that is
                // down between uses makes EVERY idle poll fail — one Error
                // per failure would pin a scary banner on the health page
                // forever. Log every failure, surface the event only after
                // several consecutive misses (a genuinely unreachable
                // phone), and recover quietly.
                self.poll_fail_streak += 1;
                eprintln!("[aokie-winbt] MAP inbox poll failed (streak {}): {e}", self.poll_fail_streak);
                if self.poll_fail_streak >= 3 && self.last_poll_error.as_deref() != Some(e.as_str()) {
                    let _ = self.event_tx.send(BluetoothEvent::Error(format!(
                        "MAP inbox poll failed {} times in a row (phone unreachable?): {e}",
                        self.poll_fail_streak
                    )));
                    self.last_poll_error = Some(e);
                }
            }
        }
    }

    fn drop_connection_state(&mut self) {
        self.mas = None;
        self.notifications_subscribed = false;
        self.subscribe_failed = false;
        self.last_poll_error = None;
        self.poll_fail_streak = 0;
    }

    /// One poll pass: list the inbox, diff against seen handles, fetch +
    /// emit each new message. Any error drops the MAS channel (the next
    /// cadence reopens it) and propagates to the caller's dedupe.
    fn poll_inbox(&mut self, phone: u64) -> Result<(), String> {
        let out = self.with_mas(phone, poll_listing_op())?;
        let listing = match out {
            OperationOutput::Listing(bytes) => bytes,
            other => return Err(format!("inbox listing op returned {other:?}")),
        };
        let entries = map_listing::parse_listing(&listing);
        let to_fetch = diff_inbox_listing(
            &entries,
            &mut self.seen_handles,
            &mut self.inbox_poll_seeded,
            self.seed_attempts,
            epoch_secs_now(),
        );
        for handle in to_fetch {
            let msg_out = self.with_mas(
                phone,
                Operation::GetMessage {
                    folder: Folder::Inbox,
                    handle: handle.clone(),
                    charset: map_mas::CHARSET_UTF8,
                },
            );
            let bytes = match msg_out {
                Ok(OperationOutput::Message(bytes)) => bytes,
                Ok(other) => return Err(format!("get-message op returned {other:?}")),
                Err(e) => {
                    // Un-see the handle so the NEXT poll retries the
                    // fetch — the dongle re-queues fetches across stall
                    // recoveries for the same reason (a stranded customer
                    // reply must not be lost to one transient failure).
                    self.seen_handles.remove(&handle);
                    return Err(e);
                }
            };
            let parsed = bmessage::parse(&bytes);
            let _ = self
                .event_tx
                .send(BluetoothEvent::SmsReceived(SmsReceivedPayload {
                    sender_phone: parsed.sender_addressing.unwrap_or_default(),
                    sender_name: parsed.sender_name,
                    body: parsed.body,
                    handle,
                    msg_type: parsed.msg_type,
                }));
        }
        Ok(())
    }

    /// Run one MAS op, reusing the pooled channel when it's open for
    /// this phone and (re)opening it otherwise. The first op on a fresh
    /// channel doubles as the SetNotificationRegistration probe when we
    /// owe one — it runs at the OBEX root and is the cheapest possible
    /// liveness proof for the new channel.
    fn with_mas(&mut self, phone: u64, op: Operation) -> Result<OperationOutput, String> {
        if self.mas.as_ref().map(|c| c.phone) != Some(phone) {
            self.mas = None;
        }
        if self.mas.is_none() {
            let subscribe_first = !self.notifications_subscribed && !self.subscribe_failed;
            let first_op = if subscribe_first {
                Operation::SetNotificationRegistration { enabled: true }
            } else {
                op.clone()
            };
            match MasConnection::open(phone, first_op) {
                Ok((conn, out)) => {
                    self.mas = Some(conn);
                    if subscribe_first {
                        self.notifications_subscribed = true;
                        let _ = self
                            .event_tx
                            .send(BluetoothEvent::MapNotificationsSubscribed);
                    } else {
                        // The open op WAS the requested op.
                        return Ok(out);
                    }
                }
                Err(e) => {
                    if subscribe_first {
                        // The phone refused the subscribe (or the channel
                        // died under it). Don't pay a second doomed
                        // CONNECT per poll; open the channel the caller
                        // actually needed and carry on without MNS —
                        // the poll is the real inbound channel anyway.
                        self.subscribe_failed = true;
                        eprintln!(
                            "[aokie-winbt] MAP notification subscribe failed ({e}) — continuing poll-only"
                        );
                        let (conn, out) = MasConnection::open(phone, op)?;
                        self.mas = Some(conn);
                        return Ok(out);
                    }
                    return Err(e);
                }
            }
        }
        let conn = self.mas.as_mut().expect("channel opened above");
        match conn.run_op(op) {
            Ok(out) => Ok(out),
            Err(e) => {
                // Any op failure poisons the channel — a half-finished
                // OBEX exchange leaves the folder/stream state unknown,
                // so the next caller starts from a clean CONNECT.
                self.mas = None;
                Err(e)
            }
        }
    }
}

/// A live MAS channel: the WinRT socket plus the pooled session that
/// remembers the OBEX Connection-Id and current folder across ops.
struct MasConnection {
    phone: u64,
    chan: BtChannel,
    session: MasMceSession,
}

impl MasConnection {
    /// Open a channel and run `first_op` end-to-end (CONNECT → … →
    /// Resting). The session stays alive for pooling.
    fn open(phone: u64, first_op: Operation) -> Result<(Self, OperationOutput), String> {
        let chan = BtChannel::connect(phone, crate::MAP_MAS_SHORT_UUID)?;
        let mut session = MasMceSession::new(first_op, MAS_INSTANCE_ID_FOR_SMS);
        session.set_keep_alive(true);
        let first = session.next_request();
        drive_mas(&chan, &mut session, first)?;
        let output = session.output().clone();
        Ok((
            Self {
                phone,
                chan,
                session,
            },
            output,
        ))
    }

    /// Queue another op on the pooled connection (SETPATH chain to the
    /// op's folder handled inside the session machine).
    fn run_op(&mut self, op: Operation) -> Result<OperationOutput, String> {
        let first = self.session.start_next_op(op)?;
        drive_mas(&self.chan, &mut self.session, Some(first))?;
        Ok(self.session.output().clone())
    }
}

/// Drive a `MasMceSession` synchronously over the blocking channel:
/// write the pending request, read one OBEX response, feed it back,
/// repeat until the op completes (Resting/Done) or fails. A `None`
/// pending request means the session is waiting for the server to
/// stream the next SRM chunk — just read again.
fn drive_mas(
    chan: &BtChannel,
    session: &mut MasMceSession,
    first: Option<Vec<u8>>,
) -> Result<(), String> {
    let mut pending = first;
    for _ in 0..MAX_OBEX_ROUND_TRIPS {
        if let Some(bytes) = pending.take() {
            chan.write_all(&bytes)?;
        }
        // Only the CONNECT response carries the 4-byte fixed payload
        // (version/flags/max-packet); every later response has none.
        let fixed = if matches!(session.state(), MasState::AwaitingConnect) {
            FixedPayload::Connect
        } else {
            FixedPayload::None
        };
        let response = read_obex_packet(chan, fixed)?;
        pending = session.handle_response(&response);
        match session.state() {
            MasState::Done | MasState::Resting => return Ok(()),
            MasState::Failed(reason) => return Err(reason.clone()),
            _ => {} // mid-op: SETPATH chain, GET continuation, or SRM stream
        }
    }
    Err(format!(
        "MAS op exceeded {MAX_OBEX_ROUND_TRIPS} round trips — treating the server as wedged"
    ))
}

/// The inbox listing op, mirrored from the dongle's PollInbox mapping:
/// both SMS type variants (the AG only stores whichever its radio
/// produces; including both costs nothing).
fn poll_listing_op() -> Operation {
    Operation::ListMessages {
        folder: Folder::Inbox,
        max_list_count: INBOX_POLL_MAX_LIST,
        filter_message_type: map_mas::MSG_TYPE_KEEP_BOTH_SMS_VARIANTS,
    }
}

/// Cadence gate: due on the first poll, then every INBOX_POLL_INTERVAL,
/// but never within POLL_SKIP_AFTER_REPLY of a successful send (the
/// Pixel MAS post-PUT settling window, mirrored from the dongle).
fn poll_due(last_poll: Option<Instant>, last_send: Option<Instant>, now: Instant) -> bool {
    if let Some(t) = last_send {
        if now.duration_since(t) < POLL_SKIP_AFTER_REPLY {
            return false;
        }
    }
    match last_poll {
        None => true,
        Some(t) => now.duration_since(t) >= INBOX_POLL_INTERVAL,
    }
}

/// Diff a fresh inbox listing against the seen-set and pick the handles
/// to fetch. Mirrors the dongle's `handle_inbox_listing` semantics:
///
/// - FIRST listing (per phone) seeds the seen-set without emitting
///   history — without this the bot would flood-reply to every existing
///   inbox row on startup. One bounded catch-up: on a first attempt
///   (`seed_attempts` == 1) the top entry only if it's UNREAD and
///   arrived within the last hour (very likely sent while our inbound
///   path was down); on a RETRY seed (`seed_attempts` >= 2, an earlier
///   attempt died before its listing landed) the top entry outright,
///   because the message that arrived during the dead window is most
///   likely sitting at the top.
/// - AFTER seeding, only newly-seen handles that are unread and recent
///   (<= 24 h) are fetched.
fn diff_inbox_listing(
    entries: &[MessageEntry],
    seen: &mut HashSet<String>,
    seeded: &mut bool,
    seed_attempts: usize,
    now_epoch: i64,
) -> Vec<String> {
    if !*seeded {
        for entry in entries {
            seen.insert(entry.handle.clone());
        }
        *seeded = true;
        let catchup = if seed_attempts >= 2 {
            entries.first()
        } else {
            entries.first().filter(|e| {
                e.read == Some(false) && entry_is_recent(e, CATCHUP_RECENT_HOURS, now_epoch)
            })
        };
        return catchup.map(|e| vec![e.handle.clone()]).unwrap_or_default();
    }
    let mut out = Vec::new();
    for entry in entries {
        if seen.insert(entry.handle.clone()) {
            // `read == None` (phone omitted the flag) is treated as
            // unread-eligible: a handle fetches at most once ever, so
            // the cost of a false positive is one harmless SmsReceived.
            if entry.read != Some(true) && entry_is_recent(entry, NEW_MSG_RECENT_HOURS, now_epoch) {
                out.push(entry.handle.clone());
            }
        }
    }
    out
}

/// Recency check against OUR epoch clock. The dongle compares local
/// wall-clock prefixes (phone and PC share a room); we go one better and
/// convert the MAP datetime to an absolute instant using the timezone
/// offset the phone itself appends ("20260713T185500+1000"), so the
/// comparison stays right even if the two clocks' zones ever disagree.
/// Entries with a missing/unparseable datetime or a missing offset PASS
/// — a phone that omits the attribute must not block the catch-up.
fn entry_is_recent(entry: &MessageEntry, hours: i64, now_epoch: i64) -> bool {
    let Some(dt) = entry.datetime.as_deref() else {
        return true;
    };
    match map_datetime_to_epoch_secs(dt) {
        Some(epoch) => epoch >= now_epoch - hours * 3600,
        None => true,
    }
}

/// Parse a MAP listing datetime ("yyyyMMddTHHmmss" + optional "±HHMM"
/// offset) into epoch seconds. Returns None when the shape is wrong or
/// the offset is absent (no way to make an absolute instant from a bare
/// local time without a TZ database, which we deliberately don't pull
/// in — no new dependencies).
fn map_datetime_to_epoch_secs(dt: &str) -> Option<i64> {
    let b = dt.as_bytes();
    if b.len() < 15 || (b[8] != b'T' && b[8] != b't') {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for &c in &b[from..to] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    let (y, mo, d) = (num(0, 4)?, num(4, 6)?, num(6, 8)?);
    let (h, mi, s) = (num(9, 11)?, num(11, 13)?, num(13, 15)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let local_secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s;
    let rest = &b[15..];
    if rest.len() < 5 || (rest[0] != b'+' && rest[0] != b'-') {
        return None;
    }
    let digits = |from: usize, to: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for &c in &rest[from..to] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    let offset = digits(1, 3)? * 3600 + digits(3, 5)? * 60;
    let signed = if rest[0] == b'+' { offset } else { -offset };
    Some(local_secs - signed)
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (Howard
/// Hinnant's public-domain algorithm) — all we need to turn the MAP
/// local timestamp into an instant once the offset is known.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Build the outbound bMessage. Mirrors the dongle's rule: `Some("MMS")`
/// selects the MMS/MIME envelope so the reply threads through Pixel's
/// RCS-fallback path; anything else (including None) is plain SMS_GSM.
fn build_outgoing_bmessage(msg_type: Option<&str>, recipient: &str, body: &str) -> Vec<u8> {
    match msg_type {
        Some(t) if t.eq_ignore_ascii_case("MMS") => bmessage::build_mms_push(recipient, body),
        _ => bmessage::build_sms_push(recipient, body),
    }
}

fn connection_snapshot(shared: &NativeShared) -> (bool, u64) {
    let connected = shared.connected.load(Ordering::Acquire);
    let phone = shared.phone_address.read().map(|p| *p).unwrap_or(0);
    (connected, phone)
}

fn epoch_secs_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(handle: &str, read: Option<bool>, datetime: Option<&str>) -> MessageEntry {
        MessageEntry {
            handle: handle.to_string(),
            datetime: datetime.map(|d| d.to_string()),
            sender_name: None,
            sender_addressing: None,
            msg_type: Some("SMS_GSM".to_string()),
            read,
        }
    }

    // A fixed "now": 2026-07-20 12:00:00 UTC.
    fn now_epoch() -> i64 {
        map_datetime_to_epoch_secs("20260720T120000+0000").unwrap()
    }

    #[test]
    fn datetime_epoch_parses_offset_and_hours() {
        let a = map_datetime_to_epoch_secs("20260719T120000+1000").unwrap();
        let b = map_datetime_to_epoch_secs("20260719T130000+1000").unwrap();
        assert_eq!(b - a, 3600, "one hour of wall time, same offset");
        // Same wall time, offsets +1000 vs -0500 differ by 15 hours.
        let c = map_datetime_to_epoch_secs("20260719T120000-0500").unwrap();
        assert_eq!(c - a, 15 * 3600);
        // No offset -> no absolute instant (callers treat as "passes").
        assert_eq!(map_datetime_to_epoch_secs("20260719T120000"), None);
        // Garbage shapes -> None, never a panic.
        assert_eq!(map_datetime_to_epoch_secs(""), None);
        assert_eq!(map_datetime_to_epoch_secs("20260719"), None);
        assert_eq!(map_datetime_to_epoch_secs("20260719T1200"), None);
        assert_eq!(map_datetime_to_epoch_secs("2026-07-19T12:00:00+1000"), None);
        assert_eq!(map_datetime_to_epoch_secs("20261319T120000+1000"), None); // month 13
        assert_eq!(map_datetime_to_epoch_secs("20260719T120000+10:00"), None);
    }

    #[test]
    fn recency_windows_match_the_two_cadence_rules() {
        let now = now_epoch();
        let e30m = entry("h1", Some(false), Some("20260720T113000+0000")); // 30 min old
        assert!(entry_is_recent(&e30m, 1, now));
        assert!(entry_is_recent(&e30m, 24, now));
        let e2h = entry("h2", Some(false), Some("20260720T100000+0000")); // 2h old
        assert!(!entry_is_recent(&e2h, 1, now));
        assert!(entry_is_recent(&e2h, 24, now));
        let e2d = entry("h3", Some(false), Some("20260718T130000+0000")); // ~47h old
        assert!(!entry_is_recent(&e2d, 1, now));
        assert!(!entry_is_recent(&e2d, 24, now));
        // Missing / unparseable / offset-less datetimes always pass.
        assert!(entry_is_recent(&entry("h4", None, None), 1, now));
        assert!(entry_is_recent(&entry("h5", None, Some("junk")), 1, now));
        assert!(entry_is_recent(
            &entry("h6", None, Some("20260720T113000")),
            1,
            now
        ));
    }

    #[test]
    fn first_seed_seeds_all_and_fetches_only_unread_recent_top() {
        let mut seen = HashSet::new();
        let mut seeded = false;
        let now = now_epoch();
        let entries = vec![
            entry("0001", Some(false), Some("20260720T113000+0000")), // unread, 30m
            entry("0002", Some(true), Some("20260720T110000+0000")),
            entry("0003", Some(false), Some("20260710T110000+0000")), // unread but old
        ];
        let fetch = diff_inbox_listing(&entries, &mut seen, &mut seeded, 1, now);
        assert!(seeded);
        assert_eq!(seen.len(), 3, "every listed handle seeded");
        assert_eq!(fetch, vec!["0001".to_string()]);
    }

    #[test]
    fn first_seed_with_read_or_stale_top_fetches_nothing() {
        let now = now_epoch();
        for top in [
            entry("0001", Some(true), Some("20260720T113000+0000")), // read
            entry("0001", Some(false), Some("20260719T100000+0000")), // unread, 26h old
        ] {
            let mut seen = HashSet::new();
            let mut seeded = false;
            let fetch = diff_inbox_listing(&[top], &mut seen, &mut seeded, 1, now);
            assert!(seeded);
            assert!(fetch.is_empty(), "no catch-up fetch for read/stale top");
        }
    }

    #[test]
    fn retry_seed_fetches_top_entry_regardless_of_flags() {
        // seed_attempts >= 2: a previous poll died before its listing —
        // the catch-up must not filter, mirroring the dongle's
        // dead-session recovery.
        let mut seen = HashSet::new();
        let mut seeded = false;
        let now = now_epoch();
        let entries = vec![
            entry("0001", Some(true), Some("20260710T110000+0000")), // read AND old
            entry("0002", Some(true), Some("20260720T113000+0000")),
        ];
        let fetch = diff_inbox_listing(&entries, &mut seen, &mut seeded, 2, now);
        assert_eq!(fetch, vec!["0001".to_string()]);
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn post_seed_fetches_new_unread_recent_skips_rest() {
        let now = now_epoch();
        let mut seen: HashSet<String> = ["0001".to_string()].into_iter().collect();
        let mut seeded = true;
        let entries = vec![
            entry("0001", Some(false), Some("20260720T113000+0000")), // already seen
            entry("0002", Some(false), Some("20260720T114500+0000")), // new, unread, recent
            entry("0003", Some(true), Some("20260720T114500+0000")),  // new but read
            entry("0004", Some(false), Some("20260718T114500+0000")), // new but 2 days old
            entry("0005", None, Some("20260720T114500+0000")),        // new, read flag unknown
        ];
        let fetch = diff_inbox_listing(&entries, &mut seen, &mut seeded, 1, now);
        assert_eq!(fetch, vec!["0002".to_string(), "0005".to_string()]);
        // Everything listed was folded into the seen-set even when not
        // fetched, so a later poll can't reconsider them.
        assert_eq!(seen.len(), 5);
    }

    #[test]
    fn poll_due_cadence_and_post_send_quiet_window() {
        let now = Instant::now();
        // First poll is always due.
        assert!(poll_due(None, None, now));
        // Inside the 45 s interval: not due.
        assert!(!poll_due(Some(now - Duration::from_secs(10)), None, now));
        // After the interval: due.
        assert!(poll_due(Some(now - Duration::from_secs(46)), None, now));
        // A send 10 s ago suppresses the poll even when the interval elapsed.
        assert!(!poll_due(
            Some(now - Duration::from_secs(46)),
            Some(now - Duration::from_secs(10)),
            now
        ));
        // A send 25 s ago does not.
        assert!(poll_due(
            Some(now - Duration::from_secs(46)),
            Some(now - Duration::from_secs(25)),
            now
        ));
    }

    #[test]
    fn outgoing_bmessage_round_trips_through_the_shared_parser() {
        let bytes = build_outgoing_bmessage(None, "+61491570156", "Hello world");
        let parsed = bmessage::parse(&bytes);
        assert_eq!(parsed.msg_type.as_deref(), Some("SMS_GSM"));
        assert_eq!(parsed.folder.as_deref(), Some("telecom/msg/outbox"));
        assert_eq!(parsed.body, "Hello world");
    }

    #[test]
    fn outgoing_bmessage_mms_selection_for_thread_parity() {
        let bytes = build_outgoing_bmessage(Some("MMS"), "+61491570156", "reply");
        let parsed = bmessage::parse(&bytes);
        assert_eq!(parsed.msg_type.as_deref(), Some("MMS"));
        // Anything other than MMS stays on the plain SMS builder.
        let bytes = build_outgoing_bmessage(Some("SMS_GSM"), "+61491570156", "reply");
        let parsed = bmessage::parse(&bytes);
        assert_eq!(parsed.msg_type.as_deref(), Some("SMS_GSM"));
    }
}
