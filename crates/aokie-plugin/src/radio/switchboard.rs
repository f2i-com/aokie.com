//! Hold/queue juggle: swap snapshots and the accept / swap-back judges.

#[allow(unused_imports)]
use super::*;

// ── Phase 4 VERIFIED SWAPS ──────────────────────────────────────────────
// AT+CHLD=2 is a TOGGLE the phone may execute, half-execute or ignore — the
// z49 live incident (2026-07-15) tore both calls down while the juggle
// assumed its second swap had worked. Nothing here assumes any more: after
// every CHLD the juggle PUMPS the radio's own event stream (through the
// same handler the main loop uses, so the tracker/status stay truthful),
// fires one on-demand AT+CLCC, and only moves session state once the phone
// itself has reported the topology. Every failure shape has an explicit
// convergence path — the line always ends in a state where someone can be
// spoken to and everyone else's session is closed honestly.

/// One point-in-time view of the phone-side call topology, assembled from
/// the indicator stream + the freshest post-swap CLCC burst.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[derive(Debug, Clone)]
pub(super) struct SwapSnapshot {
    /// callheld indicator (0 none / 1 held+active / 2 held only).
    pub(super) callheld: u64,
    /// The waiting-knock leg still up (None = knock resolved / no knock).
    pub(super) waiting_id: Option<String>,
    /// The tracker still has a current session (no terminal edge arrived).
    pub(super) current_alive: bool,
    /// Structured CLCC legs from a burst that STARTED after the settle
    /// began (None = no fresh response yet).
    pub(super) clcc: Option<Vec<ClccLeg>>,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn swap_snapshot(
    status: &Arc<RadioStatus>,
    tracker: &crate::call_session::SessionTracker,
    since: std::time::Instant,
) -> SwapSnapshot {
    let clcc = status
        .clcc_calls
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(started, _)| *started >= since)
        .map(|(_, legs)| legs.clone());
    SwapSnapshot {
        callheld: status.call_held_state.load(Ordering::Relaxed),
        waiting_id: status
            .waiting_call
            .lock()
            .unwrap()
            .as_ref()
            .map(|w| w.call_id.clone()),
        current_alive: tracker.current().is_some(),
        clcc,
    }
}

/// Pump radio events + discard mic audio until `done` says the topology
/// settled or `deadline` passes. Audio captured while the phone shuffles
/// legs is transition garbage from an ambiguous speaker — never STT input.
/// Fires one AT+CLCC after `clcc_after` so the snapshot gains authoritative
/// leg status/numbers. Returns the LAST snapshot — a timeout returns the
/// unsatisfied state and the pure judge functions own the interpretation.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn settle_swap(
    bt: &mut dyn crate::backend::RadioBackend,
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
    deadline: std::time::Duration,
    clcc_after: Option<std::time::Duration>,
    done: impl Fn(&SwapSnapshot, u16) -> bool,
) -> SwapSnapshot {
    let started = std::time::Instant::now();
    let mut clcc_fired = false;
    loop {
        // Keep the stall watchdog honest — the loop is alive, just settling.
        status.loop_beat.fetch_add(1, Ordering::Relaxed);
        while let Some(ev) = bt.try_recv_event() {
            // The CallerId RESCUE is topology-blind: a CLCC fired mid-shuffle
            // reports whichever leg is momentarily ACTIVE, and stamping that
            // number onto tracker.current() cross-labels sessions (live round
            // 4: the parked newcomer's session took the PRIMARY's number
            // during the swap-back settle — their Calls row ended up under
            // the wrong phone). Every leg's identity is already minted and
            // stable during a juggle — drop the rescue, keep everything else
            // (the structured CallListEntry burst is what verification eats).
            if matches!(ev, aokie_dongle::bluetooth::BluetoothEvent::CallerId(_)) {
                continue;
            }
            handle_event(ev, tracker, outbox, sink, status);
        }
        while bt.try_recv_audio().is_some() {}
        let snap = swap_snapshot(status, tracker, started);
        let sr = bt.get_sample_rate();
        if done(&snap, sr) || started.elapsed() >= deadline {
            return snap;
        }
        if let Some(after) = clcc_after {
            if !clcc_fired && started.elapsed() >= after {
                clcc_fired = true;
                let _ = bt.query_calls();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Verdict after the ACCEPT swap (CHLD=2 answering a knock while the
/// primary talks). Pure — unit-tested against every observed failure shape.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AcceptVerdict {
    /// Knock resolved into a held+active pair — the newcomer has the line.
    Accepted,
    /// The primary's call died mid-accept (terminal edge consumed by the
    /// settle pump) — the per-call machinery already closed it.
    PrimaryGone,
    /// The phone left ONE call held with NOBODY active — retrieve it.
    HeldAlone,
    /// The knock is still up and nothing went held: the phone ignored the
    /// CHLD — abandon the juggle, the primary still has the line.
    NothingChanged,
    /// The knock vanished without a held pair forming (the waiting caller
    /// gave up mid-swap) — the primary still has the line.
    KnockGone,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn judge_accept(snap: &SwapSnapshot, knock_id: &str) -> AcceptVerdict {
    if !snap.current_alive {
        return AcceptVerdict::PrimaryGone;
    }
    let knock_up = snap.waiting_id.as_deref() == Some(knock_id);
    match (knock_up, snap.callheld) {
        (false, 1) => AcceptVerdict::Accepted,
        (_, 2) => AcceptVerdict::HeldAlone,
        (true, _) => AcceptVerdict::NothingChanged,
        (false, _) => AcceptVerdict::KnockGone,
    }
}

/// Verdict after a swap-back CHLD=2 (two calls, no knock: a pure toggle).
/// The callheld indicator alone cannot name WHO is active afterwards (it
/// reads 1 for success AND for a no-op) — fresh CLCC evidence wins when the
/// legs' numbers allow it.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SwapBackVerdict {
    /// The primary is the active leg again; the newcomer is held.
    Swapped,
    /// The primary is active ALONE — the newcomer's leg vanished mid-swap.
    SwappedNewcomerGone,
    /// The newcomer kept the line (toggle refused/ignored); the primary is
    /// still held — degrade to single-swap.
    StayedOnNewcomer,
    /// The newcomer is active ALONE — the primary's held leg vanished.
    NewcomerAlone,
    /// Nobody is active but a held leg remains — retrieve it.
    ActiveDied,
    /// Everything tore down (the z49 signature: callheld 2→0).
    AllGone,
    /// A KNOWN number matching NEITHER party is on the active leg: the
    /// CHLD collided with a brand-new knock and the phone answered a
    /// STRANGER (live 2026-07-15 round 6 — with a waiting call up, CHLD=2
    /// accepts it instead of swapping, and this network then SACRIFICED the
    /// held primary). Assuming "Swapped" here bound the primary's session to
    /// the stranger's leg.
    StrangerActive,
    /// The CLCC evidence CONTRADICTS the callheld indicator — the phone was
    /// mid-transition when the list was captured (VoLTE swaps take 1-2s on
    /// the Pixel; live incident 2026-07-15 round 2: a 1.2s-early CLCC showed
    /// no active leg while callheld read 1, the judge concluded ActiveDied,
    /// closed the WRONG session and the follow-up "retrieve" CHLD=2 swapped
    /// the wrong caller off hold). Never act on this — re-query and judge
    /// again; persistently inconclusive resolves by the indicator alone.
    Inconclusive,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn judge_swap_back(
    snap: &SwapSnapshot,
    primary: Option<&str>,
    newcomer: Option<&str>,
) -> SwapBackVerdict {
    let suffix = |n: Option<&str>| -> Option<String> {
        n.map(crate::screen::digit_suffix).filter(|s| s.len() >= 6)
    };
    let primary = suffix(primary);
    let newcomer = suffix(newcomer);
    if let Some(clcc) = snap.clcc.as_ref() {
        let active: Vec<&ClccLeg> = clcc.iter().filter(|l| l.status == 0).collect();
        let held: Vec<&ClccLeg> = clcc.iter().filter(|l| l.status == 1).collect();
        let leg_is = |leg: &ClccLeg, who: &Option<String>| -> Option<bool> {
            match (
                leg.number.as_deref().map(crate::screen::digit_suffix),
                who.as_ref(),
            ) {
                (Some(n), Some(w)) if n.len() >= 6 => Some(n == *w),
                _ => None,
            }
        };
        // ── Consistency rules: a "someone died" conclusion is only ever
        // drawn when the CLCC and the callheld indicator AGREE. A list
        // that is missing legs the indicator says exist was captured
        // mid-transition — judging it killed the wrong session live.
        if active.is_empty() && held.is_empty() {
            return if snap.callheld == 0 {
                SwapBackVerdict::AllGone
            } else {
                SwapBackVerdict::Inconclusive
            };
        }
        if active.is_empty() {
            return if snap.callheld == 2 {
                SwapBackVerdict::ActiveDied
            } else {
                SwapBackVerdict::Inconclusive
            };
        }
        // Someone is active. A lone active leg while the indicator says a
        // held call exists is the same transitional shape.
        if held.is_empty() && snap.callheld == 1 {
            return SwapBackVerdict::Inconclusive;
        }
        let a = active[0];
        if leg_is(a, &primary) == Some(true) {
            return if held.is_empty() {
                SwapBackVerdict::SwappedNewcomerGone
            } else {
                SwapBackVerdict::Swapped
            };
        }
        if leg_is(a, &newcomer) == Some(true) {
            return if held.is_empty() {
                SwapBackVerdict::NewcomerAlone
            } else {
                SwapBackVerdict::StayedOnNewcomer
            };
        }
        // A usable number on the active leg matching NEITHER party: the
        // CHLD answered a brand-new knocker. Never treat this as a swap.
        if leg_is(a, &primary) == Some(false) && leg_is(a, &newcomer) == Some(false) {
            return SwapBackVerdict::StrangerActive;
        }
        // Active leg's number unusable (withheld) — the leg COUNTS still
        // say something: no held leg + numbers dark = someone is alone;
        // keeping the tracker's session (the newcomer) needs no further
        // CHLD, so it is the safe read.
        if held.is_empty() {
            return SwapBackVerdict::NewcomerAlone;
        }
        // 1 active + 1 held with dark numbers: indistinguishable from a
        // no-op — fall through to the indicator and trust the toggle.
    }
    resolve_swap_back_by_indicator(snap)
}

/// The indicator-only resolver (never Inconclusive) — the FINAL word when
/// CLCC evidence stays transitional/absent after a re-query.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn resolve_swap_back_by_indicator(snap: &SwapSnapshot) -> SwapBackVerdict {
    match (snap.callheld, snap.current_alive) {
        (2, _) => SwapBackVerdict::ActiveDied,
        (0, true) => SwapBackVerdict::NewcomerAlone,
        (0, false) => SwapBackVerdict::AllGone,
        _ => SwapBackVerdict::Swapped,
    }
}

/// Settle after a swap-back CHLD=2 and produce a FINAL verdict: wait for a
/// fresh CLCC (fired at 2s — VoLTE swaps take 1-2s on the live Pixel, and a
/// 1.2s query caught a mid-transition list on the first live test), judge
/// with the consistency rules, and on Inconclusive re-query ONCE before
/// falling back to the indicator. Only ever returns actionable verdicts.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn settle_and_judge_swap_back(
    bt: &mut dyn crate::backend::RadioBackend,
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
    primary: Option<&str>,
    newcomer: Option<&str>,
) -> SwapBackVerdict {
    let snap = settle_swap(
        bt,
        tracker,
        outbox,
        sink,
        status,
        std::time::Duration::from_millis(5000),
        Some(std::time::Duration::from_millis(2000)),
        |s, _| s.clcc.is_some() || (s.callheld == 0 && !s.current_alive),
    );
    let verdict = judge_swap_back(&snap, primary, newcomer);
    if verdict != SwapBackVerdict::Inconclusive {
        return verdict;
    }
    eprintln!(
        "[aokie-plugin] SWITCHBOARD: swap outcome inconclusive (transitional CLCC, callheld={}) — re-querying",
        snap.callheld
    );
    let snap2 = settle_swap(
        bt,
        tracker,
        outbox,
        sink,
        status,
        std::time::Duration::from_millis(3000),
        Some(std::time::Duration::from_millis(400)),
        |s, _| s.clcc.is_some() || (s.callheld == 0 && !s.current_alive),
    );
    let verdict2 = judge_swap_back(&snap2, primary, newcomer);
    if verdict2 != SwapBackVerdict::Inconclusive {
        return verdict2;
    }
    let resolved = resolve_swap_back_by_indicator(&snap2);
    eprintln!(
        "[aokie-plugin] SWITCHBOARD: still inconclusive after re-query — resolved by indicator: {resolved:?}"
    );
    resolved
}
