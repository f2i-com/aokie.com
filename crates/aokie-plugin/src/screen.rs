//! Call screening (spec Phase 0): deterministic number-based call policy —
//! a block list, an optional accept-pattern regex, and private-number
//! rejection — enforced at the greeting site (universally correct even on
//! phones whose caller id only arrives post-answer, like the live Pixel's
//! CLCC-only delivery). Record-driven rules (blocked Customers, whitelist)
//! live in FLOWS per the spec; this module is the fast number layer.

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
    /// Phase 1 (abuse handling): when the agent flags a caller as abusive
    /// ([[ABUSE]]), automatically append their number to the block list.
    /// Default ON; `autoBlockAbuse: false` (env AOKIE_AUTO_BLOCK_ABUSE=0)
    /// turns only the auto-block off — the notice + hangup always happen.
    pub auto_block_abuse: bool,
    /// Phase 3 (manager line): digits-only last-9 suffixes of the
    /// business's MANAGER numbers. A matching caller gets the MANAGER
    /// persona + name-inclusive lookups (READ-ONLY — caller ID is trivially
    /// spoofable, so writes will additionally require the spoken PIN) and
    /// is never screened out by the number rules.
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
        // Default ON: absence of the var (or any value but "0") auto-blocks.
        let auto_block_abuse = std::env::var("AOKIE_AUTO_BLOCK_ABUSE")
            .map(|v| v.trim() != "0")
            .unwrap_or(true);
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

    /// Phase 3: is this caller id one of the business's manager numbers?
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

    /// The screening verdict for a caller id (None/"" = withheld):
    /// `Some(reason)` means the call is screened out. Reasons are the
    /// privacy-safe codes "blocked" / "filtered" / "private".
    pub fn verdict(&self, caller_id: Option<&str>) -> Option<&'static str> {
        // Phase 3: the manager's own numbers are never screened — an accept
        // filter tuned for customer mobiles must not hang up on the boss.
        if self.is_manager(caller_id) {
            return None;
        }
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
            auto_block_abuse: true,
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

    /// Phase 3: manager numbers match on the same suffix rule, a withheld id
    /// is never a manager, and the manager's own number is never screened —
    /// even by an accept filter or the block list.
    #[test]
    fn manager_numbers_match_and_bypass_screening() {
        let p = with_manager(policy("", Some(r"^\+1"), true), "+61 400 999 888");
        assert!(p.is_manager(Some("0400999888")));
        assert!(p.is_manager(Some("+61400999888")));
        assert!(!p.is_manager(Some("0400111222")));
        assert!(!p.is_manager(None));
        assert!(!p.is_manager(Some("")));
        // The accept filter (US-only here) screens ordinary AU callers…
        assert_eq!(p.verdict(Some("0400111222")), Some("filtered"));
        // …but never the manager.
        assert_eq!(p.verdict(Some("0400999888")), None);
        // Even a block-list hit on the manager's own number loses.
        let pb = with_manager(policy("0400999888", None, false), "0400999888");
        assert_eq!(pb.verdict(Some("0400999888")), None);
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
