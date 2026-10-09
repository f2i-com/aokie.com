//! Call screening (spec Phase 0): deterministic number-based call policy —
//! a block list, an optional accept-pattern regex, and private-number
//! rejection — enforced at the greeting site (universally correct even on
//! phones whose caller id only arrives post-answer, like the live Pixel's
//! CLCC-only delivery). Record-driven rules (blocked Customers, whitelist)
//! live in FLOWS per the spec; this module is the fast number layer.
//!
//! Where the caller's number is known while the phone still rings and there
//! is no message to say to them, the call is refused there instead and never
//! picked up (`ring_verdict`): answering a blocked number to hang up on it
//! tells whoever dials it that the line is live. A reject is only a request to
//! the phone, so it is watched (`RingRefusal`): sent again when the call still
//! rings, and after that the call is answered and refused at the greeting, as
//! every screened call was before. A screened caller never rings through.

/// Parsed screening policy. Built once at radio start from the settings-fed
/// environment (`blockedNumbers` / `acceptPattern` / `rejectPrivate` /
/// `screenMessage`).
pub struct ScreenPolicy {
    /// Digits-only last-9 suffixes (the same matching rule as `phone_eq`
    /// everywhere else in the product).
    blocked: Vec<String>,
    accept: Option<regex::Regex>,
    pub reject_private: bool,
    /// Spoken to a FILTERED / PRIVATE caller before hangup ("" = silent).
    /// The polite "call back with caller ID" line.
    pub message: String,
    /// Spoken to a BLOCKED-list caller before hangup ("" = silent reject).
    /// Separate from `message`: a blocked (often abusive) number should not
    /// get the polite private-caller line.
    pub blocked_message: String,
    /// Reserved for a future operator-approved incident workflow. Model
    /// output cannot enable persistent blocking; the shipped value is false.
    pub auto_block_abuse: bool,
    /// Phase 3 (manager line): digits-only last-9 suffixes of the business's
    /// manager numbers. A match is only a PIN-challenge candidate and never
    /// changes screening, persona, lookup privacy or event authorization.
    manager: Vec<String>,
}

impl ScreenPolicy {
    /// The line to speak for a given verdict reason before hangup ("" =
    /// hang up silently). Blocked numbers get their own message.
    pub fn message_for(&self, reason: &str) -> &str {
        if reason == "blocked" {
            self.blocked_message.trim()
        } else {
            self.message.trim()
        }
    }
}

/// How long the phone has to end a ringing call it was told to reject, before
/// it is told again, and then before the call is answered instead.
pub(crate) const RING_REJECT_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);
/// The rejects a ringing call is sent in all.
pub(crate) const RING_REJECT_TRIES: u8 = 2;

/// A call being refused while it rings: the phone was told to reject it, and
/// the call is still there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RingRefusal {
    pub call_id: String,
    /// The verdict's reason ("blocked" / "filtered").
    pub reason: &'static str,
    /// When the phone was last told.
    pub sent: std::time::Instant,
    /// How many times it was told.
    pub tries: u8,
    /// The phone did not act: the call goes on to be answered and refused at
    /// the greeting, and is not looked at here again.
    pub gave_up: bool,
}

/// What to do about a call the phone was told to reject and still shows ringing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RingStep {
    /// The phone still has time to act.
    Wait,
    /// Tell it again.
    RejectAgain,
    /// It did not act: answer the call, and refuse it at the greeting.
    AnswerInstead,
}

/// The next step for a ringing call that was sent `tries` rejects, the last of
/// them `since_last` ago.
pub(crate) fn ring_refusal_step(tries: u8, since_last: std::time::Duration) -> RingStep {
    if since_last < RING_REJECT_WAIT {
        RingStep::Wait
    } else if tries < RING_REJECT_TRIES {
        RingStep::RejectAgain
    } else {
        RingStep::AnswerInstead
    }
}

/// Whether a block list as the settings hold it (`blockedNumbers`: numbers
/// apart by commas, semicolons or new lines) has `number`, by the rule the
/// policy screens callers with: the same last nine digits, of six or more.
///
/// The policy hangs up on a blocked number's CALL. This is the same list read
/// for what goes OUT: a blocked number is not texted or rung either.
pub(crate) fn list_has(list: &str, number: &str) -> bool {
    let wanted = digit_suffix(number);
    wanted.len() >= 6
        && list
            .split(|c: char| c == ',' || c == '\n' || c == ';')
            .map(digit_suffix)
            .any(|entry| entry.len() >= 6 && entry == wanted)
}

pub(crate) fn digit_suffix(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = digits.chars().count();
    if n > 9 {
        digits.chars().skip(n - 9).collect()
    } else {
        digits
    }
}

impl ScreenPolicy {
    /// A policy that screens nobody and knows no manager, with nothing read
    /// from the environment: for tests that drive the radio loop.
    #[cfg(all(test, target_os = "windows", feature = "voice"))]
    pub(crate) fn permissive() -> Self {
        Self {
            blocked: Vec::new(),
            accept: None,
            reject_private: false,
            message: String::new(),
            blocked_message: String::new(),
            auto_block_abuse: false,
            manager: Vec::new(),
        }
    }

    pub fn from_env() -> Self {
        let blocked = std::env::var("AOKIE_BLOCKED_NUMBERS")
            .unwrap_or_default()
            .split(|c: char| c == ',' || c == '\n' || c == ';')
            .map(digit_suffix)
            .filter(|s| s.len() >= 6)
            .collect();
        let accept = std::env::var("AOKIE_ACCEPT_PATTERN")
            .ok()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .and_then(|p| match regex::Regex::new(&p) {
                Ok(re) => Some(re),
                Err(e) => {
                    eprintln!(
                        "[aokie-plugin] acceptPattern is not a valid regex — ignoring it: {e}"
                    );
                    None
                }
            });
        let reject_private = std::env::var_os("AOKIE_REJECT_PRIVATE").is_some();
        let message = std::env::var("AOKIE_SCREEN_MESSAGE").unwrap_or_default();
        let blocked_message = std::env::var("AOKIE_BLOCKED_MESSAGE").unwrap_or_default();
        // Model output can end the current call but cannot persist a block.
        let auto_block_abuse = false;
        let manager = std::env::var("AOKIE_MANAGER_NUMBERS")
            .unwrap_or_default()
            .split(|c: char| c == ',' || c == '\n' || c == ';')
            .map(digit_suffix)
            .filter(|s| s.len() >= 6)
            .collect();
        Self {
            blocked,
            accept,
            reject_private,
            message,
            blocked_message,
            auto_block_abuse,
            manager,
        }
    }

    /// Phase 3: is this caller id eligible to attempt manager authentication?
    /// Same digits-only last-9-suffix rule as everything else. A withheld
    /// id is never a manager.
    pub fn is_manager(&self, caller_id: Option<&str>) -> bool {
        let suffix = digit_suffix(caller_id.unwrap_or("").trim());
        suffix.len() >= 6 && self.manager.iter().any(|m| *m == suffix)
    }

    /// Phase 1 auto-block: add a number to the RUNNING policy (the caller's
    /// next attempt is screened immediately, before any persistence lands).
    /// Returns false for an unusable id (withheld / too few digits) or a
    /// number already on the list.
    pub fn block_number(&mut self, raw: &str) -> bool {
        let suffix = digit_suffix(raw);
        if suffix.len() < 6 || self.blocked.iter().any(|b| *b == suffix) {
            return false;
        }
        self.blocked.push(suffix);
        true
    }

    pub fn is_active(&self) -> bool {
        !self.blocked.is_empty() || self.accept.is_some() || self.reject_private
    }

    /// The verdict for a call that is still RINGING: `Some(reason)` when it is
    /// to be refused without being picked up. Only for a call to us, whose
    /// number is known already (a withheld number is told from a late one only
    /// after the answer, so "private" is never decided here), and only when
    /// there is no message to say to that caller: saying one needs the call
    /// answered, and the greeting site does that.
    pub fn ring_verdict(&self, outbound: bool, caller_id: Option<&str>) -> Option<&'static str> {
        if outbound {
            return None;
        }
        let id = caller_id.map(str::trim).filter(|id| !id.is_empty())?;
        let reason = self.verdict(Some(id))?;
        self.message_for(reason).is_empty().then_some(reason)
    }

    /// The screening verdict for a caller id (None/"" = withheld):
    /// `Some(reason)` means the call is screened out. Reasons are the
    /// privacy-safe codes "blocked" / "filtered" / "private".
    pub fn verdict(&self, caller_id: Option<&str>) -> Option<&'static str> {
        let id = caller_id.unwrap_or("").trim();
        if id.is_empty() {
            return if self.reject_private {
                Some("private")
            } else {
                None
            };
        }
        let suffix = digit_suffix(id);
        if suffix.len() >= 6 && self.blocked.iter().any(|b| *b == suffix) {
            return Some("blocked");
        }
        if let Some(re) = &self.accept {
            if !re.is_match(id) {
                return Some("filtered");
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(blocked: &str, accept: Option<&str>, private: bool) -> ScreenPolicy {
        ScreenPolicy {
            blocked: blocked
                .split(',')
                .map(digit_suffix)
                .filter(|s| s.len() >= 6)
                .collect(),
            accept: accept.map(|p| regex::Regex::new(p).unwrap()),
            reject_private: private,
            message: String::new(),
            blocked_message: String::new(),
            auto_block_abuse: false,
            manager: Vec::new(),
        }
    }

    fn with_manager(mut p: ScreenPolicy, numbers: &str) -> ScreenPolicy {
        p.manager = numbers
            .split(',')
            .map(digit_suffix)
            .filter(|s| s.len() >= 6)
            .collect();
        p
    }

    /// Phase 3: manager candidates match on the same suffix rule, a withheld
    /// id is ineligible, and ANI never bypasses ordinary screening.
    #[test]
    fn manager_numbers_are_candidates_but_do_not_bypass_screening() {
        let p = with_manager(policy("", Some(r"^\+1"), true), "+61 400 999 888");
        assert!(p.is_manager(Some("0400999888")));
        assert!(p.is_manager(Some("+61400999888")));
        assert!(!p.is_manager(Some("0400111222")));
        assert!(!p.is_manager(None));
        assert!(!p.is_manager(Some("")));
        // The accept filter (US-only here) screens ordinary AU callers…
        assert_eq!(p.verdict(Some("0400111222")), Some("filtered"));
        // …and the same result applies to a spoofable manager candidate.
        assert_eq!(p.verdict(Some("0400999888")), Some("filtered"));
        // A block-list hit is also authoritative.
        let pb = with_manager(policy("0400999888", None, false), "0400999888");
        assert_eq!(pb.verdict(Some("0400999888")), Some("blocked"));
        // is_active unaffected: manager numbers alone don't turn screening on.
        let quiet = with_manager(policy("", None, false), "0400999888");
        assert!(!quiet.is_active());
    }

    #[test]
    fn message_for_picks_the_right_line() {
        let mut p = policy("0491570156", None, true);
        p.message = "polite".into();
        p.blocked_message = "no abuse".into();
        assert_eq!(p.message_for("blocked"), "no abuse");
        assert_eq!(p.message_for("private"), "polite");
        assert_eq!(p.message_for("filtered"), "polite");
        // Blank blocked message = silent reject even when the polite one is set.
        p.blocked_message = String::new();
        assert_eq!(p.message_for("blocked"), "");
    }

    #[test]
    fn blocked_numbers_match_across_formats() {
        let p = policy("+61 491 570 156, 0400111222", None, false);
        assert_eq!(p.verdict(Some("0491570156")), Some("blocked"));
        assert_eq!(p.verdict(Some("+61491570156")), Some("blocked"));
        assert_eq!(p.verdict(Some("(04) 0011-1222")), Some("blocked"));
        assert_eq!(p.verdict(Some("0499999999")), None);
        // Short fragments never block (worse than no filter at all).
        let short = policy("243", None, false);
        assert_eq!(short.verdict(Some("0491570156")), None);
    }

    /// The list as the settings hold it, read for what goes out: the same
    /// rule as a caller's (formats, fragments), and nothing but the list (no
    /// accept pattern: that is about who is answered when they call).
    #[test]
    fn a_settings_list_has_a_number_however_either_is_written() {
        let list = "+61 491 570 156, 0400111222\n(03) 9000 1234; 243";
        assert!(list_has(list, "0491570156"));
        assert!(list_has(list, "+61491570156"));
        assert!(list_has(list, " (04) 0011-1222 "));
        assert!(list_has(list, "+61 3 9000 1234"));
        assert!(!list_has(list, "0499999999"));
        // A fragment on the list blocks nobody, and a fragment is never blocked.
        assert!(!list_has(list, "0491570243"));
        assert!(!list_has(list, "243"));
        assert!(!list_has("", "0491570156"));
        assert!(!list_has(list, ""));
        // What the policy says of a caller, the list says of a recipient.
        let p = policy("+61 491 570 156, 0400111222", None, false);
        for n in ["0491570156", "+61400111222", "0499999999"] {
            assert_eq!(p.verdict(Some(n)) == Some("blocked"), list_has("+61 491 570 156, 0400111222", n), "{n}");
        }
    }

    #[test]
    fn accept_pattern_filters_non_matching_ids() {
        // Australian mobiles only.
        let p = policy("", Some(r"^(\+?61|0)4"), false);
        assert_eq!(p.verdict(Some("0491570156")), None);
        assert_eq!(p.verdict(Some("+61491570156")), None);
        assert_eq!(p.verdict(Some("0299998888")), Some("filtered"));
        assert_eq!(p.verdict(Some("+14155550100")), Some("filtered"));
        // A withheld id is NOT the pattern's business (that's rejectPrivate).
        assert_eq!(p.verdict(None), None);
    }

    /// A screened caller is refused while the phone rings only when their
    /// number is known and nothing is to be said to them; everyone else is
    /// left to the answer and the greeting site, as before.
    #[test]
    fn a_ringing_call_is_refused_only_for_a_known_screened_number_with_no_message() {
        let mut p = policy("0491570156", Some(r"^(\+?61|0)4"), true);
        assert_eq!(p.ring_verdict(false, Some("+61491570156")), Some("blocked"));
        assert_eq!(p.ring_verdict(false, Some(" 0299998888 ")), Some("filtered"));
        // Someone who is answered.
        assert_eq!(p.ring_verdict(false, Some("0400111222")), None);
        // No number yet: withheld or merely late is not known while it rings.
        assert_eq!(p.ring_verdict(false, None), None);
        assert_eq!(p.ring_verdict(false, Some("  ")), None);
        // A call we placed is never screened.
        assert_eq!(p.ring_verdict(true, Some("0491570156")), None);
        // With a line to say to them, the call is answered to say it.
        p.blocked_message = "This number is not accepted.".into();
        assert_eq!(p.ring_verdict(false, Some("0491570156")), None);
        assert_eq!(p.ring_verdict(false, Some("0299998888")), Some("filtered"));
        p.message = "Please call back showing your number.".into();
        assert_eq!(p.ring_verdict(false, Some("0299998888")), None);
    }

    /// A reject is a request: the phone has a moment to act on it, is told once
    /// more, and after that the call is answered to be refused at the greeting.
    #[test]
    fn a_reject_the_phone_does_not_act_on_is_sent_again_and_then_the_call_is_answered_instead() {
        use std::time::Duration;
        let just_under = RING_REJECT_WAIT - Duration::from_millis(1);
        assert_eq!(ring_refusal_step(1, Duration::ZERO), RingStep::Wait);
        assert_eq!(ring_refusal_step(1, just_under), RingStep::Wait);
        assert_eq!(ring_refusal_step(1, RING_REJECT_WAIT), RingStep::RejectAgain);
        assert_eq!(ring_refusal_step(RING_REJECT_TRIES, just_under), RingStep::Wait);
        assert_eq!(ring_refusal_step(RING_REJECT_TRIES, RING_REJECT_WAIT), RingStep::AnswerInstead);
        // Every try is used before the call is answered, and the whole of it is a ring or so, not a minute.
        let mut tries = 1;
        while ring_refusal_step(tries, RING_REJECT_WAIT) == RingStep::RejectAgain {
            tries += 1;
        }
        assert_eq!(tries, RING_REJECT_TRIES);
        assert!(RING_REJECT_WAIT * u32::from(RING_REJECT_TRIES) <= Duration::from_secs(4));
    }

    #[test]
    fn private_numbers_screen_only_when_enabled() {
        let p = policy("", None, true);
        assert_eq!(p.verdict(None), Some("private"));
        assert_eq!(p.verdict(Some("")), Some("private"));
        assert_eq!(p.verdict(Some("0491570156")), None);
        let off = policy("", None, false);
        assert_eq!(off.verdict(None), None);
    }

    #[test]
    fn blocked_wins_over_accept() {
        let p = policy("0491570156", Some(r"^0"), false);
        assert_eq!(p.verdict(Some("0491570156")), Some("blocked"));
    }

    /// Phase 1 auto-block: a flagged number screens IMMEDIATELY on the
    /// running policy — format-agnostic, deduped, and unusable ids refused.
    #[test]
    fn block_number_applies_live_and_dedupes() {
        let mut p = policy("", None, false);
        assert!(!p.is_active());
        assert!(p.block_number("+61 400 111 222"));
        assert!(p.is_active());
        assert_eq!(p.verdict(Some("0400111222")), Some("blocked"));
        // Same number in another format = already blocked, not a new entry.
        assert!(!p.block_number("0400111222"));
        // Withheld / fragment ids can never be blocked.
        assert!(!p.block_number(""));
        assert!(!p.block_number("243"));
    }
}
