//! PBAP (Phonebook Access Profile) one-shot fetch over the WinRT RFCOMM
//! channel — pull the phonebook once per phone connect, emit
//! `ContactsFetched`.
//!
//! Phase 2 of `docs/NATIVE_BLUETOOTH_TRANSPORT_PLAN.md` ("PBAP phonebook
//! pull — same reuse: `pbap.rs`, `vcard.rs` over RFCOMM"). The contacts
//! feed the plugin's known-caller personalization (name lookup by
//! number), so one pull per connection is enough — phones don't hot-add
//! contacts mid-call often enough to justify polling.
//!
//! REUSED VERBATIM from `aokie_radio`:
//!   - `obex.rs`  — packet/header codec (via `rfcomm::read_obex_packet`)
//!   - `pbap.rs`  — `PbapPceSession`: CONNECT (PBAP target UUID) →
//!                  SETPATH telecom → SETPATH pb → GET x-bt/phonebook,
//!                  including the Pixel/Bluedroid SRM streaming latch
//!                  that stalls naive clients mid-phonebook
//!   - `vcard.rs` — the vCard 2.1 parser (line folding, quoted-printable)
//!
//! REWRITTEN (deliberately): the dongle's `pbap_runtime.rs` is
//! event-driven over its own RFCOMM mux; here a compact synchronous
//! driver loop runs the SAME session machine over the blocking
//! `BtChannel` (see rfcomm.rs's blocking-by-design note).

#![allow(dead_code)] // Phase 2 — worker wiring lands with the calls/audio engines.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aokie_bluetooth::aokie_radio::obex::FixedPayload;
use aokie_bluetooth::aokie_radio::pbap::{PbapPceSession, PbapState};
use aokie_bluetooth::aokie_radio::vcard::Contact;
use aokie_dongle::bluetooth::{BluetoothEvent, ContactPair};

use crate::rfcomm::{read_obex_packet, BtChannel, MAX_OBEX_ROUND_TRIPS};
use crate::runtime::NativeShared;

/// Hard cap on emitted (number, name) pairs — the phonebook is read
/// into memory whole, so bound what a hostile/bloated address book can
/// make us hold and ship.
const MAX_CONTACTS: usize = 2000;

/// One initial attempt plus ONE retry per phone connection, then we
/// give up until the next connect (a phone without PBAP or with the
/// service held by another app would otherwise be re-poked forever).
const PBAP_MAX_ATTEMPTS: usize = 2;

/// Delay before the retry — long enough that a phone mid-connecting its
/// own services has settled, short enough that contacts still arrive
/// early in the first call.
const PBAP_RETRY_BACKOFF: Duration = Duration::from_secs(10);

/// One-shot phonebook pull, driven from `MapLoop::tick` (same worker
/// thread, same cadence). Self-gating: does nothing until the phone is
/// connected, and at most PBAP_MAX_ATTEMPTS tries per connection.
pub(crate) struct PbapFetch {
    /// Phone address we've finished with this connection (fetched or
    /// attempts exhausted). Reset on disconnect so the next connect
    /// refetches.
    done_for: Option<u64>,
    attempts: usize,
    backoff_until: Option<Instant>,
}

impl PbapFetch {
    pub(crate) fn new() -> Self {
        Self {
            done_for: None,
            attempts: 0,
            backoff_until: None,
        }
    }

    /// Called from MapLoop::tick (same thread/cadence); runs the
    /// one-shot fetch when the phone is connected and not yet fetched.
    pub(crate) fn tick(&mut self, event_tx: &Sender<BluetoothEvent>, shared: &Arc<NativeShared>) {
        let _connected = shared.connected.load(Ordering::Acquire);
        let phone = shared.phone_address.read().map(|p| *p).unwrap_or(0);
        if phone == 0 {
            // No phone known: re-arm so the next connection gets a fresh pull.
            self.done_for = None;
            self.attempts = 0;
            self.backoff_until = None;
            return;
        }
        if self.done_for == Some(phone) {
            return;
        }
        if let Some(t) = self.backoff_until {
            if Instant::now() < t {
                return;
            }
        }
        match fetch_phonebook(phone) {
            Ok(pairs) => {
                eprintln!(
                    "[aokie-winbt] PBAP fetch: {} contact pair(s) from {:012X}",
                    pairs.len(),
                    phone
                );
                let _ = event_tx.send(BluetoothEvent::ContactsFetched(pairs));
                self.done_for = Some(phone);
            }
            Err(e) => {
                self.attempts += 1;
                if self.attempts >= PBAP_MAX_ATTEMPTS {
                    let _ = event_tx.send(BluetoothEvent::Error(format!(
                        "PBAP phonebook fetch failed after {} attempt(s): {e}",
                        self.attempts
                    )));
                    self.done_for = Some(phone);
                } else {
                    eprintln!(
                        "[aokie-winbt] PBAP fetch failed ({e}) — one retry in {}s",
                        PBAP_RETRY_BACKOFF.as_secs()
                    );
                    self.backoff_until = Some(Instant::now() + PBAP_RETRY_BACKOFF);
                }
            }
        }
    }
}

/// CONNECT → SETPATH telecom/pb → GET phonebook → flatten to pairs.
fn fetch_phonebook(phone: u64) -> Result<Vec<ContactPair>, String> {
    let chan = BtChannel::connect(phone, crate::PBAP_PSE_SHORT_UUID)?;
    let mut session = PbapPceSession::new();
    drive_pbap(&chan, &mut session)?;
    Ok(flatten_contacts(session.into_contacts(), MAX_CONTACTS))
}

/// Drive a `PbapPceSession` synchronously over the blocking channel —
/// same shape as map.rs's drive_mas: write the pending request, read
/// one OBEX response, feed it back, repeat until Done/Failed. A `None`
/// pending request means the PSE is streaming the next SRM chunk; just
/// read again.
fn drive_pbap(chan: &BtChannel, session: &mut PbapPceSession) -> Result<(), String> {
    let mut pending = session.next_request();
    for _ in 0..MAX_OBEX_ROUND_TRIPS {
        if let Some(bytes) = pending.take() {
            chan.write_all(&bytes)?;
        }
        // Only the CONNECT response carries the 4-byte fixed payload.
        let fixed = if matches!(session.state(), PbapState::AwaitingConnect) {
            FixedPayload::Connect
        } else {
            FixedPayload::None
        };
        let response = read_obex_packet(chan, fixed)?;
        pending = session.handle_response(&response);
        match session.state() {
            PbapState::Done => return Ok(()),
            PbapState::Failed(reason) => return Err(reason.clone()),
            _ => {} // mid-op: SETPATH chain, GET continuation, or SRM stream
        }
    }
    Err(format!(
        "PBAP fetch exceeded {MAX_OBEX_ROUND_TRIPS} round trips — treating the server as wedged"
    ))
}

/// One ContactPair per TEL field (a vCard with two numbers yields two
/// pairs — the event contract's documented shape), capped at `cap`.
fn flatten_contacts(contacts: Vec<Contact>, cap: usize) -> Vec<ContactPair> {
    let mut out = Vec::new();
    for contact in contacts {
        for number in contact.phone_numbers {
            if out.len() >= cap {
                return out;
            }
            out.push(ContactPair {
                phone_number: number,
                display_name: contact.display_name.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(name: &str, numbers: &[&str]) -> Contact {
        Contact {
            display_name: name.to_string(),
            phone_numbers: numbers.iter().map(|n| n.to_string()).collect(),
        }
    }

    #[test]
    fn flatten_expands_multiple_numbers_per_card() {
        let pairs = flatten_contacts(
            vec![
                contact("Alice", &["+61411111111", "+61422222222"]),
                contact("Bob", &["+61433333333"]),
            ],
            MAX_CONTACTS,
        );
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].phone_number, "+61411111111");
        assert_eq!(pairs[0].display_name, "Alice");
        assert_eq!(pairs[1].phone_number, "+61422222222");
        assert_eq!(pairs[2].display_name, "Bob");
    }

    #[test]
    fn flatten_honours_the_cap() {
        let cards: Vec<Contact> = (0..10)
            .map(|i| contact(&format!("Person {i}"), &[&format!("+614000000{i:02}")]))
            .collect();
        let pairs = flatten_contacts(cards, 3);
        assert_eq!(pairs.len(), 3);
    }

    #[test]
    fn flatten_empty_phonebook_is_empty_not_error() {
        assert!(flatten_contacts(Vec::new(), MAX_CONTACTS).is_empty());
    }
}
