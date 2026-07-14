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
    /// Spoken to a screened caller before hangup ("" = hang up silently).
    pub message: String,
}

fn digit_suffix(raw: &str) -> String {
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
        Self {
            blocked,
            accept,
            reject_private,
            message,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.blocked.is_empty() || self.accept.is_some() || self.reject_private
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
        }
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
}
