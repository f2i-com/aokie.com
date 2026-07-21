//! [[LOOKUP]] mid-call business lookups: flow.run plumbing and trigger heuristics.

#[allow(unused_imports)]
use super::*;

/// One in-flight read-only `business-lookup` host request. Realtime calls poll
/// it without blocking the radio/SCO loop; the legacy responder may still use
/// the bounded wait after playing its audible filler. Every failure path is
/// converted to an explicit UNAVAILABLE result so the model never guesses.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) struct PendingBusinessLookup {
    pub(super) host: Arc<crate::host_rpc::HostRpc>,
    pub(super) id: Option<u64>,
    pub(super) rx: std::sync::mpsc::Receiver<crate::host_rpc::HostResult>,
    pub(super) deadline: Instant,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
impl Drop for PendingBusinessLookup {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.host.forget(id);
        }
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn begin_business_lookup(
    host: &Arc<crate::host_rpc::HostRpc>,
    sink: &mut dyn Sink,
    question: &str,
    call_id: &str,
    from: &str,
    manager: bool,
) -> Option<PendingBusinessLookup> {
    // `manager` is true only after this call context passed the deterministic
    // PIN gate and its ANI remained an eligible manager candidate. The host
    // may include customer names only in that authenticated case.
    let params = serde_json::json!({
        "flowSlug": "business-lookup",
        "input": { "question": question, "callId": call_id, "from": from, "manager": manager },
        "correlationId": call_id,
        "idempotencyKey": format!("aokie:{call_id}:lookup:{}", uuid::Uuid::new_v4().simple()),
        "timeoutMs": 6000,
    });
    let (id, line, rx) = host.begin("flow.run", params);
    if sink.send_line(&line).is_err() {
        host.forget(id);
        return None;
    }
    Some(PendingBusinessLookup {
        host: Arc::clone(host),
        id: Some(id),
        rx,
        // Slightly beyond the host's own 6000 ms flow timeout so a slow-but-
        // successful lookup is not locally abandoned as "timed out" while the
        // host is still delivering its result.
        deadline: Instant::now() + std::time::Duration::from_millis(6500),
    })
}

/// FloorManager ownership (guide P1-8, PROMOTED 2026-07-14 after a day of
/// shadow telemetry): the fused decision drives the substantive yield + duck
/// actions. `AOKIE_FLOOR_SHADOW_ONLY=1` reverts to observe-only with the
/// legacy threshold chain — the escape hatch for live tuning.
pub(super) fn floor_manager_owns() -> bool {
    static OWNS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OWNS.get_or_init(|| std::env::var_os("AOKIE_FLOOR_SHADOW_ONLY").is_none())
}

/// True when a reply ANNOUNCES a data check to the caller ("let me check the
/// calendar for...") — used as a marker-forgotten fallback: the model's
/// spoken history strips markers, so it imitated its own announce-only turns
/// and left callers in dead air waiting on a check that never ran (call
/// acadcecc — twice in one call for "the 16th of August").
pub(super) fn looks_like_lookup_announcement(reply: &str) -> bool {
    let r = reply.to_lowercase();
    [
        "let me check",
        "let me look",
        "let me pull up",
        "i'll check",
        "i will check",
        "checking the calendar",
        "checking our records",
    ]
    .iter()
    .any(|p| r.contains(p))
}

/// True when the CALLER explicitly asks for a live check ("can you look it
/// up?", "check the calendar") - a reply without a lookup marker then runs
/// one on their words instead of deferring (call 372836dc).
pub(super) fn caller_asked_for_lookup(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "look it up",
        "look that up",
        "look up the",
        "check the calendar",
        "check the availability",
        "check availability",
        "can you check",
        "could you check",
    ]
    .iter()
    .any(|p| t.contains(p))
}

/// True when a reply CLAIMS availability ("...looks open", "we have
/// availability") — combined with a dated caller question and NO lookup this
/// is a hallucinated calendar claim (call 2c00cac0: 'Monday 10 August looks
/// open' asserted from thin air); the lookup fallback verifies it.
pub(super) fn looks_like_availability_claim(reply: &str) -> bool {
    let r = reply.to_lowercase();
    [
        "looks open",
        "is available",
        "we have availability",
        "looks free",
        "is free on",
        "have an opening",
    ]
    .iter()
    .any(|p| r.contains(p))
}

/// True when a reply DEFERS to the team ("I'll have the team confirm...") —
/// combined with a date in the caller's words this is the model taking its
/// safe exit instead of running the lookup it has (call c01b7dcf: 'what
/// about twenty first of August?' → 'I'll have the team confirm' until the
/// caller said 'can you look it up please').
pub(super) fn looks_like_team_deferral(reply: &str) -> bool {
    let r = reply.to_lowercase();
    (r.contains("the team") || r.contains("our team"))
        && (r.contains("confirm") || r.contains("check") || r.contains("get back"))
}

/// True when caller text plausibly names a calendar date: a month name on a
/// word boundary, an ISO date, or a day number with an ordinal suffix
/// ("21st"). Deliberately loose — a false positive only costs one read-only
/// lookup — but bounded so "do you do catering?" never triggers.
pub(super) fn mentions_a_date(text: &str) -> bool {
    let t = format!(" {} ", text.to_lowercase());
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    for m in MONTHS {
        if let Some(pos) = t.find(m) {
            let before_ok = t[..pos]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphabetic());
            let after_ok = t[pos + m.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphabetic());
            if before_ok && after_ok {
                return true;
            }
        }
    }
    let chars: Vec<char> = t.chars().collect();
    for i in 0..chars.len() {
        if !chars[i].is_ascii_digit() {
            continue;
        }
        // ISO "2026-08-21".
        let ten: String = chars[i..].iter().take(10).collect();
        if ten.chars().count() == 10
            && ten.chars().enumerate().all(|(j, c)| {
                if j == 4 || j == 7 {
                    c == '-'
                } else {
                    c.is_ascii_digit()
                }
            })
        {
            return true;
        }
        // "21st" / "3rd" / "22nd".
        let mut k = i;
        while k < chars.len() && chars[k].is_ascii_digit() {
            k += 1;
        }
        let suf: String = chars[k..].iter().take(2).collect();
        if k - i <= 2 && matches!(suf.as_str(), "st" | "nd" | "rd" | "th") {
            return true;
        }
    }
    false
}

#[cfg(test)]
pub(super) mod lookup_announcement_tests {
    use super::looks_like_lookup_announcement as ann;
    use super::looks_like_team_deferral as defer;
    use super::mentions_a_date as dateish;

    #[test]
    fn announce_phrases_detected() {
        assert!(ann(
            "Let me check the calendar for the 16th of August for you."
        ));
        assert!(ann("Aye, I'll check our records for that date, matey."));
        assert!(ann("One second - checking the calendar now."));
    }

    #[test]
    fn ordinary_replies_do_not_trigger() {
        assert!(!ann(
            "Saturday 8 August looks open. Would you like me to put a booking request in?"
        ));
        assert!(!ann("We're open Monday to Friday, nine to five."));
        assert!(!ann("I'll have the team confirm that for you."));
    }

    #[test]
    fn caller_lookup_requests_detected() {
        use super::caller_asked_for_lookup as ask;
        assert!(ask("Can you look it up, please? Yeah."));
        assert!(ask("check the calendar for me"));
        assert!(!ask("I'll look for parking"));
        assert!(!ask("what's on the menu?"));
    }

    #[test]
    fn availability_claims_detected() {
        use super::looks_like_availability_claim as claim;
        assert!(claim(
            "Monday 10 August looks open. Would you like me to book it?"
        ));
        assert!(claim("We have availability on Friday."));
        assert!(!claim("Let me check the calendar for you."));
        assert!(!claim("We are open from 11am to 9pm daily."));
    }

    #[test]
    fn team_deferral_phrases_detected() {
        assert!(defer(
            "I'll have the team confirm the exact availability for you, mate."
        ));
        assert!(defer("The team will check and get back to you."));
        assert!(!defer(
            "Saturday 22 August looks open. Would you like me to put a booking request in?"
        ));
        assert!(!defer("Our chef makes it fresh daily."));
    }

    #[test]
    fn dateish_caller_text_detected() {
        assert!(dateish("what about twenty first of August?"));
        assert!(dateish("anything on the 21st?"));
        assert!(dateish("availability 2026-08-21"));
        assert!(!dateish("do you do catering?"));
        assert!(!dateish("maybe later")); // 'may' inside a word never counts
    }
}

/// Returns `(digest_text, spoken)`. `spoken` is a ready-to-speak sentence the
/// FLOW composed deterministically for date-availability questions — when it
/// is present the caller hears it VERBATIM and no LLM round runs (live calls
/// 1defd805 + b58274ed: the 9B model was handed a digest whose DIRECT ANSWER
/// line said the date was open/booked and still told the caller "that date
/// isn't in our current booking window" — records-composed speech is the same
/// pattern the SMS loop already uses).
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn finish_business_lookup(pending: Option<PendingBusinessLookup>) -> (String, Option<String>) {
    let Some(mut pending) = pending else {
        return ("LOOKUP UNAVAILABLE (host offline)".to_string(), None);
    };
    let left = pending.deadline.saturating_duration_since(Instant::now());
    match pending.rx.recv_timeout(left) {
        Ok(result) => {
            // HostRpc removes the request id before delivering the response.
            pending.id = None;
            decode_business_lookup_result(result)
        }
        Err(_) => {
            // PendingBusinessLookup::drop forgets the still-live request id.
            eprintln!("[aokie-plugin] lookup flow timed out");
            ("LOOKUP UNAVAILABLE (timed out)".to_string(), None)
        }
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn poll_business_lookup(
    pending: &mut PendingBusinessLookup,
    now: Instant,
) -> Option<(String, Option<String>)> {
    match pending.rx.try_recv() {
        Ok(result) => {
            // HostRpc removes the request id before delivering the response.
            pending.id = None;
            Some(decode_business_lookup_result(result))
        }
        Err(std::sync::mpsc::TryRecvError::Empty) if now < pending.deadline => None,
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            eprintln!("[aokie-plugin] lookup flow timed out");
            Some(("LOOKUP UNAVAILABLE (timed out)".to_string(), None))
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            pending.id = None;
            eprintln!("[aokie-plugin] lookup flow response channel closed");
            Some(("LOOKUP UNAVAILABLE".to_string(), None))
        }
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn decode_business_lookup_result(result: crate::host_rpc::HostResult) -> (String, Option<String>) {
    match result {
        Ok(v) => {
            // The desktop runner's success status is "done" (the same
            // vocabulary flow_run_logs persists); "succeeded" kept for other
            // hosts. Checking ONLY "succeeded" + the host dropping `result`
            // from the RPC response meant every live lookup injected
            // LOOKUP UNAVAILABLE while a perfect digest sat in the run log.
            let status = v.get("status").and_then(serde_json::Value::as_str);
            let ok = matches!(status, Some("done") | Some("succeeded"));
            let digest = v
                .get("result")
                .and_then(|r| r.get("digest").or_else(|| r.get("answer")))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let spoken = v
                .get("result")
                .and_then(|r| r.get("spoken"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if ok && !digest.trim().is_empty() {
                (digest.trim().to_string(), spoken)
            } else {
                eprintln!("[aokie-plugin] lookup flow returned no digest (status ok: {ok})");
                ("LOOKUP UNAVAILABLE (no result)".to_string(), None)
            }
        }
        Err(e) => {
            eprintln!("[aokie-plugin] lookup flow failed: {e}");
            ("LOOKUP UNAVAILABLE".to_string(), None)
        }
    }
}

/// Fire-and-forget llama KV prefix warm (2026-07-14, ring pre-warm follow-up):
/// discover + 1-token-process the reply prefix on a WORKER thread. Called at
/// ring (base persona) and again when the call-scoped overlay lands (its
/// persona replaces the prefix, so the first warm no longer matches).
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn spawn_llm_prefix_warm(
    agent_endpoint: &Arc<Mutex<Option<String>>>,
    agent_model: Option<String>,
    status: &Arc<RadioStatus>,
    system_prompt: String,
    history: Vec<serde_json::Value>,
    label: &'static str,
    handoff: Option<Arc<Mutex<Option<crate::agent::LlmClient>>>>,
) {
    let ep = agent_endpoint.clone();
    let st = status.clone();
    let _ = std::thread::Builder::new()
        .name("aokie-llm-warm".into())
        .spawn(move || {
            let configured = ep.lock().unwrap().clone();
            let Some(endpoint) = crate::agent::discover_endpoint(configured.as_deref()) else {
                return;
            };
            *st.llm_error.lock().unwrap() = None;
            let client = crate::agent::LlmClient::new(endpoint, agent_model);
            // Park a connected client for the loop to adopt (first-turn
            // speculation + replies skip the lazy connect entirely).
            if let Some(slot) = handoff {
                *slot.lock().unwrap() = Some(client.clone());
            }
            let mut messages =
                vec![serde_json::json!({ "role": "system", "content": system_prompt })];
            messages.extend(history);
            let t = std::time::Instant::now();
            match client.warm_prefix(serde_json::json!(messages)) {
                Ok(()) => eprintln!(
                    "[aokie-plugin] llm prefix warmed ({label}) in {:?}",
                    t.elapsed()
                ),
                Err(e) => eprintln!("[aokie-plugin] llm prefix warm ({label}) failed: {e}"),
            }
        });
}
