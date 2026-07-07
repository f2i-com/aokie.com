//! Match a caller's free-form service name against the configured
//! service catalogue. Conservative on purpose — when a phrase doesn't
//! cleanly identify ONE service we'd rather return `None` than guess
//! and book the wrong appointment.
//!
//! Used by both the user-utterance pre-detector ("I'd like a lawn mow"
//! → fills draft.service) and the bot-reply detector ("let me check
//! Friday for the lawn mow" → resolves which service the bot was
//! talking about).

use crate::calendar::Service;

/// Find the single service whose name matches `phrase`, or `None` if
/// no clear winner emerges. Returns the catalogue's canonical name as
/// the second tuple element so callers store the configured spelling
/// rather than whatever the caller said.
///
/// Algorithm:
///  1. Exact match (case-insensitive) — wins immediately.
///  2. Substring match either way — caller's needle inside service
///     name, or service name inside caller's needle.
///  3. Token-overlap — services whose tokens are all present in the
///     phrase score highest; ties become `None`.
pub fn match_service<'a>(services: &'a [Service], phrase: &str) -> Option<&'a Service> {
    let phrase_norm = normalize(phrase);
    if phrase_norm.is_empty() {
        return None;
    }
    let actives: Vec<&Service> = services.iter().filter(|s| s.active).collect();

    // 1. Exact
    if let Some(svc) = actives
        .iter()
        .find(|s| normalize(&s.name) == phrase_norm)
        .copied()
    {
        return Some(svc);
    }

    // 2. Substring (either direction). Two passes — normal (with
    //    spaces preserved), then "compact" (spaces stripped) so
    //    "haircut" matches "hair cut + style" via "haircutstyle"
    //    containment. Compact-only matches are scored lower so a
    //    cleaner space-preserving match wins ties.
    let phrase_compact = compact(&phrase_norm);
    let substring_hits: Vec<&Service> = actives
        .iter()
        .copied()
        .filter(|s| {
            let n = normalize(&s.name);
            if phrase_norm.contains(&n) || n.contains(&phrase_norm) {
                return true;
            }
            let n_compact = compact(&n);
            phrase_compact.contains(&n_compact) || n_compact.contains(&phrase_compact)
        })
        .collect();
    if substring_hits.len() == 1 {
        return Some(substring_hits[0]);
    }
    if substring_hits.len() > 1 {
        // Score: services CONTAINED IN phrase rank above phrases
        // contained in service (the caller said the full service name
        // — strong signal). Tie-break on longer service token count
        // (more specific). If still tied, return None for ambiguity.
        let scored: Vec<(&Service, i32)> = substring_hits
            .iter()
            .map(|s| {
                let n = normalize(&s.name);
                let direction = if phrase_norm.contains(&n) { 100 } else { 0 };
                (*s, direction + token_count(&n) as i32)
            })
            .collect();
        let best = scored.iter().map(|(_, sc)| *sc).max().unwrap_or(0);
        let top: Vec<&Service> = scored
            .iter()
            .filter(|(_, sc)| *sc == best)
            .map(|(s, _)| *s)
            .collect();
        if top.len() == 1 {
            return Some(top[0]);
        }
        // Multiple top-scorers — fall through to token overlap,
        // which uses a stricter ratio gate.
    }

    // 3. Token overlap with a 50% ratio threshold. "+" / "and" are
    //    filler. A service is a hit when at least half its core tokens
    //    appear as words in the phrase. Highest-ratio service wins;
    //    ties (e.g. "trim" matching both "Hedge trim" and "Hair trim"
    //    at 0.5 each) are ambiguous and return None.
    let phrase_tokens: Vec<&str> = phrase_norm.split_whitespace().collect();
    let mut token_hits: Vec<(&Service, f32)> = Vec::new();
    for svc in &actives {
        let svc_norm = normalize(&svc.name);
        let svc_tokens: Vec<&str> = svc_norm.split_whitespace().collect();
        if svc_tokens.is_empty() {
            continue;
        }
        let core_tokens: Vec<&str> = svc_tokens
            .iter()
            .copied()
            .filter(|t| !is_filler_token(t))
            .collect();
        if core_tokens.is_empty() {
            continue;
        }
        let matched = core_tokens
            .iter()
            .filter(|t| phrase_tokens.iter().any(|p| *p == **t))
            .count();
        if matched == 0 {
            continue;
        }
        let ratio = matched as f32 / core_tokens.len() as f32;
        if ratio >= 0.5 {
            token_hits.push((*svc, ratio));
        }
    }
    if !token_hits.is_empty() {
        let best = token_hits.iter().map(|(_, r)| *r).fold(0.0_f32, f32::max);
        let top: Vec<&Service> = token_hits
            .iter()
            .filter(|(_, r)| (r - best).abs() < 0.01)
            .map(|(s, _)| *s)
            .collect();
        if top.len() == 1 {
            return Some(top[0]);
        }
        // Genuinely ambiguous — caller said "trim" and we have both
        // "Hedge trim" and "Hair trim". Bail out so the bot can ask.
    }

    None
}

/// Lowercase, collapse non-alphanumeric runs to single spaces, trim.
/// Aligns punctuation differences ("hair-cut" vs "hair cut") and
/// drops the "+" / "&" connectors so caller phrasing doesn't have to
/// match the catalogue's typography.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;
    for c in s.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() {
            out.push(lc);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_string()
}

fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn token_count(s: &str) -> usize {
    s.split_whitespace().count()
}

fn is_filler_token(t: &str) -> bool {
    matches!(t, "and" | "or" | "with" | "a" | "the")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str) -> Service {
        Service {
            id: 1,
            name: name.into(),
            duration_minutes: 30,
            description: None,
            active: true,
            sort_order: 0,
            created_at: 0,
            requires_address: false,
        }
    }

    #[test]
    fn exact_case_insensitive() {
        let services = vec![svc("Lawn mow")];
        assert_eq!(
            match_service(&services, "lawn mow").unwrap().name,
            "Lawn mow"
        );
        assert_eq!(
            match_service(&services, "LAWN MOW").unwrap().name,
            "Lawn mow"
        );
        assert_eq!(
            match_service(&services, "Lawn Mow").unwrap().name,
            "Lawn mow"
        );
    }

    #[test]
    fn substring_caller_inside_service() {
        // Caller said "lawn mow"; catalogue is "Lawn mow + edges".
        let services = vec![svc("Lawn mow + edges")];
        assert_eq!(
            match_service(&services, "lawn mow").unwrap().name,
            "Lawn mow + edges"
        );
    }

    #[test]
    fn substring_service_inside_caller() {
        // Caller embedded service in a longer phrase.
        let services = vec![svc("Lawn mow")];
        assert_eq!(
            match_service(&services, "I'd like a lawn mow please")
                .unwrap()
                .name,
            "Lawn mow"
        );
    }

    #[test]
    fn token_overlap_picks_correct_service_with_filler_words() {
        let services = vec![svc("Hedge trim"), svc("Lawn mow")];
        // "trim my hedges" — token "hedge" matches, picks Hedge trim.
        // (Note: "hedges" doesn't tokenize to "hedge" — this test
        // documents the limit. We don't lemmatize; "hedges" needs to
        // appear verbatim in the service name to match by token.)
        assert!(match_service(&services, "I want my hedges trimmed").is_none());
        // But "hedge trim" matches by substring path:
        assert_eq!(
            match_service(&services, "hedge trim please").unwrap().name,
            "Hedge trim"
        );
    }

    #[test]
    fn ambiguity_returns_none() {
        let services = vec![svc("Hedge trim"), svc("Hair trim")];
        // "trim" alone matches both via token overlap (since "trim"
        // is the only core token of each); ambiguous → None.
        assert!(match_service(&services, "I want a trim").is_none());
    }

    #[test]
    fn punctuation_normalised() {
        // Catalogue spelled as one word — caller can say "hair-cut",
        // "haircut", or "Haircut" and they all reach it. Compact
        // substring (spaces/punct stripped) catches the hyphenated case.
        let services = vec![svc("Haircut")];
        assert_eq!(
            match_service(&services, "I want a haircut").unwrap().name,
            "Haircut"
        );
        assert_eq!(
            match_service(&services, "hair-cut tomorrow").unwrap().name,
            "Haircut"
        );
        assert_eq!(
            match_service(&services, "Haircut please").unwrap().name,
            "Haircut"
        );
    }

    #[test]
    fn inactive_services_ignored() {
        let mut s = svc("Lawn mow");
        s.active = false;
        assert!(match_service(&[s], "lawn mow").is_none());
    }

    #[test]
    fn empty_phrase_returns_none() {
        let services = vec![svc("Lawn mow")];
        assert!(match_service(&services, "").is_none());
        assert!(match_service(&services, "   ").is_none());
    }
}
