//! `docs/contracts/transfer/`: every fixture parsed with the plugin's own
//! types. The same files are the OAIY repository's fixtures; the digests in
//! `SHA256SUMS` are what the two sides compare.

use super::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn folder() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/contracts/transfer")
}

pub(crate) fn fixture(name: &str) -> Value {
    let path = folder().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{name} is not JSON: {error}"))
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("{value} is not a list"))
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect()
}

/// The digest the harness computes: SHA-256 of the bytes with CRLF folded to
/// LF, so a checkout with core.autocrlf and the committed blob agree.
fn digest(name: &str) -> String {
    let bytes = std::fs::read(folder().join(name)).unwrap();
    let text = String::from_utf8(bytes).unwrap().replace("\r\n", "\n");
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

const FIXTURES: [&str; 8] = [
    "transfer-v1.tool-call.fixture.json",
    "transfer-v1.tool-result.fixture.json",
    "transfer-v1.outcome.fixture.json",
    "transfer-v1.cancel.fixture.json",
    "transfer-v1.start-ready.fixture.json",
    "transfer-v1.ring-plan.fixture.json",
    "transfer-v1.reserved-offer-id.fixture.json",
    "transfer-v1.caller-asked.fixture.json",
];

#[test]
fn every_file_is_this_contracts_and_the_digests_are_current() {
    for name in FIXTURES {
        let value = fixture(name);
        assert_eq!(value["contract"], "transfer_v1", "{name}");
        assert!(value["kind"].is_string(), "{name}");
    }
    // Nothing else in the folder that the tests do not know about.
    let mut on_disk: Vec<String> = std::fs::read_dir(folder())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    on_disk.sort();
    let mut expected: Vec<String> = FIXTURES.iter().map(|name| name.to_string()).collect();
    expected.extend(["SHA256SUMS".to_string(), "transfer-v1.md".to_string()]);
    expected.sort();
    assert_eq!(on_disk, expected, "a file was added or removed without the tests noticing");

    // SHA256SUMS lists every fixture, and every digest is current. The
    // document transfer-v1.md is not listed: scripts/check-contracts.mjs
    // compares it, like the fixtures, with the OAIY copy.
    let sums = std::fs::read_to_string(folder().join("SHA256SUMS")).unwrap();
    let mut listed = std::collections::BTreeMap::new();
    for line in sums.lines().filter(|line| !line.trim().is_empty()) {
        let (hash, name) = line.split_once("  ").expect("`<sha256>  <file>` per line");
        listed.insert(name.to_string(), hash.to_string());
    }
    let files: Vec<String> = FIXTURES.iter().map(|name| name.to_string()).collect();
    for name in &files {
        assert_eq!(
            listed.get(name),
            Some(&digest(name)),
            "SHA256SUMS is stale for {name}: run `node scripts/check-contracts.mjs --write-transfer-sums`"
        );
    }
    assert_eq!(listed.len(), files.len());
}

#[test]
fn the_tool_call_fixture_names_and_arguments_are_judged_by_the_plugin_as_written() {
    let fixture = fixture("transfer-v1.tool-call.fixture.json");
    assert_eq!(fixture["toolName"], TOOL_NAME);
    assert_eq!(fixture["frame"]["name"], TOOL_NAME);
    assert_eq!(fixture["frame"]["type"], "formlogic.realtime.tool_call");
    for name in strings(&fixture["validToolNames"]) {
        assert!(crate::realtime_voice::is_tool_name(&name), "{name:?}");
    }
    for name in strings(&fixture["invalidToolNames"]) {
        assert!(!crate::realtime_voice::is_tool_name(&name), "{name:?}");
    }
    let cases = fixture["arguments"]["cases"].as_array().unwrap();
    assert!(cases.len() >= 12);
    for case in cases {
        let parsed = parse_arguments(&case["arguments"]);
        if case["accepted"] == true {
            assert_eq!(parsed.map(Reason::as_str), Some(case["reason"].as_str().unwrap()), "{case}");
        } else {
            assert_eq!(parsed, None, "{case}");
        }
    }
    // The frame's own argument is one of the accepted ones.
    assert!(parse_arguments(&fixture["frame"]["arguments"]).is_some());
}

#[test]
fn the_tool_result_fixture_is_what_the_plugin_says() {
    let fixture = fixture("transfer-v1.tool-result.fixture.json");
    let ringing_case = &fixture["ringing"];
    let answer = ringing(
        ringing_case["requestId"].as_str().unwrap(),
        ringing_case["ringSeconds"].as_u64().unwrap(),
    );
    assert!(answer.ok);
    let mut expected = fixture["frame"]["output"].clone();
    // The instruction is fixed text and informative: it must be there, and
    // the plugin's own is what the fixture shows.
    assert_eq!(answer.output["instruction"], expected["instruction"]);
    expected["instruction"] = answer.output["instruction"].clone();
    assert_eq!(answer.output, expected);
    assert_eq!(fixture["frame"]["ok"], true);
    assert_eq!(fixture["frame"]["name"], TOOL_NAME);

    assert_eq!(strings(&fixture["planReasons"]), PLAN_REASONS);
    assert_eq!(strings(&fixture["pluginReasons"]), {
        let mut reasons: Vec<String> = PLUGIN_REASONS.iter().map(|r| r.to_string()).collect();
        reasons.push("call_changed".into());
        reasons
    });
    for case in fixture["refusals"].as_array().unwrap() {
        let status = match case["status"].as_str().unwrap() {
            "refused" => RefusalStatus::Refused,
            "unavailable" => RefusalStatus::Unavailable,
            other => panic!("unknown status {other}"),
        };
        let reason = case["reason"].as_str().unwrap();
        let answer = refusal(status, reason);
        assert!(!answer.ok);
        assert_eq!(answer.output["status"], case["status"], "{case}");
        assert_eq!(answer.output["reason"], case["reason"], "{case}: the reason is in the closed set");
        assert!(answer.output["instruction"].as_str().unwrap().len() > 20);
    }
    // Every reason of both closed sets has a case.
    let cased: Vec<String> = fixture["refusals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["reason"].as_str().unwrap().to_string())
        .collect();
    for reason in PLAN_REASONS
        .iter()
        .chain(PLUGIN_REASONS.iter())
        .filter(|reason| **reason != "busy")
        .chain(["call_changed"].iter())
    {
        assert!(cased.contains(&reason.to_string()), "no case for {reason}");
    }
}

#[test]
fn the_outcome_fixture_frames_are_the_plugins_frames() {
    let fixture = fixture("transfer-v1.outcome.fixture.json");
    assert_eq!(
        strings(&fixture["outcomes"]),
        ["accepted", "declined", "unavailable", "expired", "cancelled"]
    );
    let max = fixture["maxMessageChars"].as_u64().unwrap() as usize;
    assert_eq!(max, MAX_MESSAGE_CHARS);
    let mut seen = std::collections::BTreeSet::new();
    for case in fixture["cases"].as_array().unwrap() {
        let frame = &case["frame"];
        assert_eq!(frame["type"], "formlogic.realtime.transfer_outcome");
        assert!(frame["callId"].is_string() && frame["generation"].is_u64());
        let parsed: OutcomeFrame = serde_json::from_value(frame.clone()).unwrap();
        seen.insert(parsed.outcome.as_str());
        // What the plugin would send for it: the frame's own members, in the
        // shape the stream stamps (type, callId, generation).
        let mut sent = serde_json::to_value(&parsed).unwrap();
        for stamped in ["type", "callId", "generation"] {
            sent[stamped] = frame[stamped].clone();
        }
        assert_eq!(&sent, frame, "{case}");
        // A message exists only on declined, and is already what the plugin
        // would have let through.
        match (&parsed.message, parsed.outcome) {
            (Some(message), Outcome::Declined) => {
                assert!(message.chars().count() <= max);
                assert_eq!(sanitize_owner_message(message).as_deref(), Some(message.as_str()));
            }
            (None, _) => {}
            other => panic!("a message on {other:?}"),
        }
    }
    assert_eq!(seen.len(), 5, "one case per outcome: {seen:?}");
    assert_eq!(fixture["timings"]["mediaSetupSeconds"], 45);
    assert_eq!(fixture["timings"]["resolutionGraceSeconds"], 10);
    assert_eq!(fixture["timings"]["planWaitMillis"], 1500);
}

#[test]
fn the_reserved_offer_id_fixture_matches_the_derivation_and_the_key_thumbprints() {
    let fixture = fixture("transfer-v1.reserved-offer-id.fixture.json");
    assert_eq!(fixture["prefix"], RESERVED_OFFER_PREFIX);
    let request_id = fixture["requestId"].as_str().unwrap();
    for phone in fixture["testPhones"].as_array().unwrap() {
        // The thumbprint follows from the public key.
        let key: Vec<u8> = (0..64)
            .step_by(2)
            .map(|at| u8::from_str_radix(&phone["publicKeyHex"].as_str().unwrap()[at..at + 2], 16).unwrap())
            .collect();
        let key: [u8; 32] = key.try_into().unwrap();
        let thumbprint = aokie_protocol::v2::EndpointPublicKey::from_ed25519_bytes(&key).thumbprint;
        assert_eq!(phone["holderThumbprint"], thumbprint.as_str(), "{phone}");
        // And each generation's id follows from the request and the thumbprint.
        for (generation, expected) in strings(&phone["offerIds"]).iter().enumerate() {
            let id = reserved_offer_id(request_id, &thumbprint, generation as u32);
            assert_eq!(&id, expected, "generation {generation} of {phone}");
            assert_eq!(id.len(), fixture["idLength"].as_u64().unwrap() as usize);
            assert!(is_reserved_offer_id(&id));
        }
    }
    for other in strings(&fixture["notReservedIds"]) {
        assert!(!is_reserved_offer_id(&other), "{other:?}");
    }
}

/// The union of every closed word the plugin sends or reads is in the contract
/// document, so a word added in code without the document fails here.
#[test]
fn the_reason_vocabulary_in_the_contract_names_every_word_the_plugin_uses() {
    let document = std::fs::read_to_string(folder().join("transfer-v1.md")).unwrap();
    let in_document = |word: &str| document.contains(&format!("`{word}`"));
    let mut words: Vec<String> = PLAN_REASONS.iter().map(|word| word.to_string()).collect();
    words.extend(PLUGIN_REASONS.iter().map(|word| word.to_string()));
    words.extend(
        ["call_changed", "tool_limit", "unsupported", "not_offered", "no_answer", "transfer_in_progress"].map(str::to_string),
    );
    words.extend(["caller_asked", "urgent", "policy_rule"].map(str::to_string));
    words.extend(
        [Outcome::Accepted, Outcome::Declined, Outcome::Unavailable, Outcome::Expired, Outcome::Cancelled]
            .map(|outcome| outcome.as_str().to_string()),
    );
    words.extend(
        [CancelReason::OwnerDeclined, CancelReason::MessageInstead, CancelReason::GaveUp]
            .map(|reason| reason.as_str().to_string()),
    );
    words.extend([Notice::TooLate, Notice::UnknownRequest].map(|notice| notice.as_str().to_string()));
    words.extend(["return", "failback", "refused", "unavailable", STOP_HANDOFF_TAKEOVER].map(str::to_string));
    words.extend(["transferred"].map(str::to_string));
    for word in &words {
        assert!(in_document(word), "`{word}` is not in transfer-v1.md");
    }
    // The other way: the plugin answers with no word the document does not list.
    for status in [RefusalStatus::Refused, RefusalStatus::Unavailable] {
        for reason in PLAN_REASONS.iter().chain(PLUGIN_REASONS.iter()) {
            let answer = refusal(status, reason);
            assert_eq!(answer.output["reason"], *reason);
            assert!(in_document(answer.output["reason"].as_str().unwrap()));
        }
        // An unknown word from a host is not passed on.
        assert_eq!(refusal(status, "the model may say anything")
            .output["reason"], "plan_unavailable");
    }
}

/// Second review F7: the document and the architecture note state the ring
/// default the parser applies, and the relay refusals the gateway gives.
#[test]
fn the_documents_state_the_ring_default_and_the_relay_refusals_the_code_gives() {
    let document = std::fs::read_to_string(folder().join("transfer-v1.md")).unwrap().replace("\r\n", "\n");
    assert_eq!(RING_SECONDS_DEFAULT, 40);
    assert_eq!((RING_SECONDS_MIN, RING_SECONDS_MAX), (20, 90));
    assert!(document.contains("20 to 90 s; 40 s when the plan says nothing about it"));
    let architecture = std::fs::read_to_string(folder().join("../../ARCHITECTURE.md")).unwrap().replace("\r\n", "\n");
    assert!(architecture.contains("20 to 90 s, the plan's (40 when it says nothing)"));
    // The relay answers an accept from outside the plan `transfer_unavailable`,
    // and only the decline `not_a_target` (companion_gateway tests pin both).
    assert!(document.contains("an accept with `transfer_unavailable`"));
    assert!(document.contains("a decline with\n`not_a_target`"));
    assert!(!document.contains("refuses an accept or a decline\nfrom any other device (`not_a_target`)"));
    assert!(document.contains("whole gateway connection (a reconnect)"));
    assert!(document.contains("**request card**"));
}

/// Review R9: the contract says what is true today about delivery. Nothing
/// launches a Windows Companion, and no host posts a ring hint or wakes a
/// phone; the reserved offer id is defined and vector-tested and that is all.
/// Words that promised more must not come back, in the document, the fixtures
/// or the notes beside them.
#[test]
fn the_contract_does_not_promise_a_launch_a_wake_or_a_ring_hint_that_no_host_gives() {
    let read = |name: &str| std::fs::read_to_string(folder().join(name)).unwrap().replace("\r\n", "\n");
    let document = read("transfer-v1.md");
    let ring_plan = read("transfer-v1.ring-plan.fixture.json");
    let reserved = read("transfer-v1.reserved-offer-id.fixture.json");
    let architecture = std::fs::read_to_string(folder().join("../../ARCHITECTURE.md")).unwrap().replace("\r\n", "\n");
    for (name, text) in [
        ("transfer-v1.md", &document),
        ("the ring-plan fixture", &ring_plan),
        ("the reserved-offer-id fixture", &reserved),
        ("ARCHITECTURE.md", &architecture),
    ] {
        for promised in [
            "the toast starts it",
            "the toast exists to start a Companion",
            "Windows Companion it starts",
            "ring hint posted by the\nhost names",
            "ring hint the OAIY host posts",
            "so it can wake phones",
            "so it can start delivery",
            "The ring hint carries generation 0",
        ] {
            assert!(!text.contains(promised), "{name} promises: {promised}");
        }
    }
    // What is true instead, said where a reader would look.
    assert!(document.contains("Nothing launches a Companion today"));
    assert!(document.contains("**No host posts a ring\nhint yet**"));
    assert!(document.contains("A phone in the roster is assumed reachable\nfor the whole ring window"));
    assert!(ring_plan.contains("No host does yet"));
    assert!(reserved.contains("No host posts a ring hint yet"));
    assert!(architecture.contains("no host wakes a phone or posts a ring hint yet"));
}

#[test]
fn the_cancel_fixture_is_what_the_plugin_reads_and_answers() {
    let fixture = fixture("transfer-v1.cancel.fixture.json");
    // The closed set of reasons is the plugin's.
    assert_eq!(
        strings(&fixture["cancel"]["reasons"]),
        [CancelReason::OwnerDeclined, CancelReason::MessageInstead, CancelReason::GaveUp]
            .map(|reason| reason.as_str().to_string())
    );
    for reason in strings(&fixture["cancel"]["reasons"]) {
        assert!(fixture["cancel"]["reasonMeaning"][&reason].is_string(), "{reason}");
    }
    for case in fixture["cancel"]["cases"].as_array().unwrap() {
        let (request_id, reason) = parse_cancel(&case["frame"]).unwrap_or_else(|| panic!("{case}"));
        assert_eq!(request_id, case["frame"]["requestId"].as_str().unwrap());
        assert_eq!(reason.as_str(), case["frame"]["reason"].as_str().unwrap());
        assert_eq!(case["frame"]["type"], "formlogic.realtime.transfer_cancel");
    }
    assert_eq!(fixture["cancel"]["cases"].as_array().unwrap().len(), 3, "a case per reason");
    for case in fixture["cancel"]["ignored"]["cases"].as_array().unwrap() {
        assert_eq!(parse_cancel(&case["frame"]), None, "{case}");
    }
    // The notices are the plugin's, each described, each shown once.
    let notices = [Notice::TooLate, Notice::UnknownRequest].map(|notice| notice.as_str().to_string());
    assert_eq!(strings(&fixture["notice"]["notices"]), notices);
    for notice in &notices {
        assert!(fixture["notice"]["noticeMeaning"][notice].is_string(), "{notice}");
        assert!(
            fixture["notice"]["cases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|case| case["frame"]["notice"] == notice.as_str()),
            "{notice} has a case"
        );
    }
    for case in fixture["notice"]["cases"].as_array().unwrap() {
        let frame = &case["frame"];
        assert_eq!(frame["type"], "formlogic.realtime.transfer_notice");
        let parsed: NoticeFrame = serde_json::from_value(frame.clone()).unwrap();
        assert!(parsed.at_ms > 1_000_000_000_000, "atMs is epoch milliseconds");
    }
    assert_eq!(fixture["behaviour"]["cases"].as_array().unwrap().len(), 6);
}

/// The check the file describes (its `algorithm`), run from the file's own
/// data alone: no pattern, list or number of the plugin's is used, only its
/// normaliser (which has its own test).
fn caller_asked_from_the_file(fixture: &Value, said: &[String]) -> bool {
    let person = fixture["person"].as_str().unwrap();
    let head = fixture["head"].as_str().unwrap();
    let compile = |source: &Value, flags: &str| {
        let pattern = source.as_str().unwrap().replace("<person>", person).replace("<head>", head);
        regex::Regex::new(&format!("{flags}{pattern}")).unwrap()
    };
    let list = |key: &str| -> Vec<regex::Regex> {
        fixture[key].as_array().unwrap().iter().map(|source| compile(source, "")).collect()
    };
    let (rules, blocks, turn_blocks) = (list("rules"), list("blocks"), list("turnBlocks"));
    let role_marker = compile(&fixture["roleMarker"], "(?i)");
    let fillers = strings(&fixture["fillers"]);
    let ends: Vec<char> = strings(&fixture["sentenceEnds"])
        .iter()
        .map(|end| end.chars().next().unwrap())
        .collect();
    let recent = fixture["recentTurns"].as_u64().unwrap() as usize;
    let chars = fixture["turnChars"].as_u64().unwrap() as usize;
    let removed: Vec<char> = strings(&fixture["removedCharacters"])
        .iter()
        .flat_map(|removed| removed.chars())
        .collect();
    let plain = |text: &str| -> String {
        let text: String = text.chars().filter(|character| !removed.contains(character)).collect();
        phrase::normalize(&text)
            .split(' ')
            .filter(|word| !word.is_empty() && !fillers.iter().any(|filler| filler == word))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let unfinished = &fixture["unfinished"];
    let (always, bare) = (compile(&unfinished["always"], ""), compile(&unfinished["bare"], ""));
    let hard: Vec<char> = strings(&unfinished["hard"]).iter().map(|end| end.chars().next().unwrap()).collect();
    let trailing: Vec<char> =
        strings(&unfinished["trailing"]).iter().map(|end| end.chars().next().unwrap()).collect();
    let trailing_dots = unfinished["trailingDots"].as_u64().unwrap() as usize;
    // How a run of end characters ends the sentence before it.
    let ending_of = |run: &[char]| -> &'static str {
        if run.iter().any(|character| hard.contains(character)) {
            "hard"
        } else if run.iter().any(|character| trailing.contains(character))
            || run.iter().filter(|character| **character == '.').count() >= trailing_dots
        {
            "trailing"
        } else {
            "stop"
        }
    };
    // The sentences of a turn as it was said, each made plain, with how it ends.
    let sentences = |turn: &str| -> Vec<(String, &'static str)> {
        let mut found = Vec::new();
        let mut current = String::new();
        let characters: Vec<char> = turn.chars().collect();
        let mut at = 0;
        while at < characters.len() {
            if !ends.contains(&characters[at]) {
                current.push(characters[at]);
                at += 1;
                continue;
            }
            let mut run = vec![characters[at]];
            while at + 1 < characters.len() && ends.contains(&characters[at + 1]) {
                at += 1;
                run.push(characters[at]);
            }
            at += 1;
            found.push((std::mem::take(&mut current), ending_of(&run)));
        }
        found.push((current, "open"));
        found
            .into_iter()
            .map(|(text, ending)| (plain(&text), ending))
            .filter(|(text, _)| !text.is_empty())
            .collect()
    };
    let is_unfinished = |sentence: &str, ending: &str| -> bool {
        match ending {
            "hard" => false,
            "stop" => always.is_match(sentence),
            _ => always.is_match(sentence) || bare.is_match(sentence),
        }
    };
    let mut carried: Option<String> = None;
    for turn in &said[said.len().saturating_sub(recent)..] {
        let skip = turn.chars().count().saturating_sub(chars);
        let turn: String = turn.chars().skip(skip).collect();
        let whole = plain(&turn);
        if role_marker.is_match(&turn)
            || whole.is_empty()
            || turn_blocks.iter().any(|block| block.is_match(&whole))
        {
            carried = None;
            continue;
        }
        for (sentence, ending) in sentences(&turn) {
            let read = match carried.take() {
                Some(before) => format!("{before} {sentence}"),
                None => sentence,
            };
            if !blocks.iter().any(|block| block.is_match(&read))
                && rules.iter().any(|rule| rule.is_match(&read))
            {
                return true;
            }
            if is_unfinished(&read, ending) {
                carried = Some(read);
            }
        }
    }
    false
}

/// The file's `backchannel` rule, from the file's own words.
fn is_backchannel_from_the_file(fixture: &Value, text: &str) -> bool {
    let backchannel = &fixture["backchannel"];
    let (words, pairs) = (strings(&backchannel["words"]), strings(&backchannel["pairs"]));
    let at_most = backchannel["atMost"].as_u64().unwrap() as usize;
    let pieces: Vec<String> = text
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || ['-', '\u{2010}', '\u{2011}', '\u{2013}'].contains(&c))
        .map(|piece| piece.chars().filter(|c| c.is_alphanumeric()).collect::<String>())
        .filter(|piece| !piece.is_empty())
        .collect();
    let (mut at, mut said) = (0, 0);
    while at < pieces.len() {
        if pieces.get(at + 1).is_some_and(|next| pairs.contains(&format!("{} {next}", pieces[at]))) {
            at += 2;
        } else if words.contains(&pieces[at]) {
            at += 1;
        } else {
            return false;
        }
        said += 1;
    }
    (1..=at_most).contains(&said)
}

#[test]
fn the_caller_asked_fixture_passes_and_its_patterns_are_the_plugins_patterns() {
    let fixture = fixture("transfer-v1.caller-asked.fixture.json");
    let turns = |case: &Value| strings(&case["turns"]);
    let cases = |key: &str| fixture[key].as_array().unwrap().clone();
    let (positive, negative, window) = (cases("positive"), cases("negative"), cases("window"));
    assert_eq!((positive.len(), negative.len(), window.len()), (91, 108, 4));
    for case in &positive {
        assert!(caller_asked(&turns(case)), "{case}");
    }
    for case in &negative {
        assert!(!caller_asked(&turns(case)), "{case}");
    }
    for case in &window {
        assert_eq!(caller_asked(&turns(case)), case["asked"] == true, "{case}");
    }
    // Nothing is a known gap any more: every case there would be a negative one.
    let gaps = fixture["knownGaps"]["cases"].as_array().unwrap();
    assert!(gaps.is_empty(), "a known gap is a case the check gets wrong; list it or fix it");

    // The backchannel: the words, the pairs and the limit are the plugin's, its
    // text rule decides every listed turn as the file says, the turns that
    // remain are the ones the plugin keeps, and the check then decides as listed.
    let backchannel = &fixture["backchannel"];
    assert_eq!(backchannel["atMost"], phrase::ACK_AT_MOST);
    assert_eq!(strings(&backchannel["words"]), phrase::ACK_WORDS);
    assert_eq!(strings(&backchannel["pairs"]), phrase::ACK_PAIRS);
    let (acknowledgements, not_acknowledgements) = (
        strings(&backchannel["acknowledgements"]),
        strings(&backchannel["notAcknowledgements"]),
    );
    assert_eq!((acknowledgements.len(), not_acknowledgements.len()), (29, 18));
    for said in &acknowledgements {
        assert!(phrase::is_backchannel(said), "{said:?}");
        assert!(is_backchannel_from_the_file(&fixture, said), "{said:?}");
    }
    for said in &not_acknowledgements {
        assert!(!phrase::is_backchannel(said), "{said:?}");
        assert!(!is_backchannel_from_the_file(&fixture, said), "{said:?}");
    }
    let backchannel_cases = backchannel["cases"].as_array().unwrap();
    assert_eq!(backchannel_cases.len(), 7);
    for case in backchannel_cases {
        let kept = caller_turns(&turns(case));
        assert_eq!(kept, strings(&case["window"]), "{case}");
        assert_eq!(caller_asked(&kept), case["asked"] == true, "{case}");
    }

    // Every number, list and pattern of the file is the plugin's, and the
    // file's own algorithm, run from its own data, decides every case the way
    // the plugin does: neither can move without the other.
    assert_eq!(fixture["recentTurns"], phrase::RECENT_TURNS);
    assert_eq!(fixture["turnChars"], phrase::TURN_CHARS);
    assert_eq!(fixture["person"], phrase::PERSON);
    assert_eq!(fixture["head"], phrase::HEAD);
    assert_eq!(strings(&fixture["rules"]), phrase::RULES);
    assert_eq!(strings(&fixture["blocks"]), phrase::BLOCKS);
    assert_eq!(strings(&fixture["turnBlocks"]), phrase::TURN_BLOCKS);
    assert_eq!(fixture["roleMarker"], phrase::ROLE_MARKER);
    let unfinished = &fixture["unfinished"];
    assert_eq!(unfinished["always"], phrase::UNFINISHED_ALWAYS);
    assert_eq!(unfinished["bare"], phrase::UNFINISHED_BARE);
    let single = |key: &str| -> Vec<char> {
        strings(&unfinished[key]).iter().map(|end| end.chars().next().unwrap()).collect()
    };
    assert_eq!(single("hard"), phrase::HARD_ENDS);
    assert_eq!(single("trailing"), phrase::TRAILING_ENDS);
    assert_eq!(unfinished["trailingDots"], phrase::TRAILING_DOTS);
    assert_eq!(
        strings(&fixture["removedCharacters"]),
        vec![phrase::SOFT_HYPHEN.to_string()],
        "the characters the normaliser removes outright"
    );
    assert_eq!(strings(&fixture["fillers"]), phrase::FILLERS);
    let ends: Vec<char> =
        strings(&fixture["sentenceEnds"]).iter().map(|end| end.chars().next().unwrap()).collect();
    assert_eq!(ends, phrase::SENTENCE_ENDS);
    let listed: std::collections::BTreeSet<char> = regex::Regex::new(r"U\+([0-9A-F]{4})")
        .unwrap()
        .captures_iter(fixture["normalise"].as_str().unwrap())
        .map(|found| char::from_u32(u32::from_str_radix(&found[1], 16).unwrap()).unwrap())
        .collect();
    assert_eq!(listed, phrase::APOSTROPHES.iter().copied().collect(), "the file's apostrophes");
    let every_case = positive.iter().chain(&negative).chain(&window);
    for case in every_case {
        let turns = turns(case);
        assert_eq!(caller_asked_from_the_file(&fixture, &turns), caller_asked(&turns), "{case}");
    }
    for case in backchannel_cases {
        let window = strings(&case["window"]);
        assert_eq!(caller_asked_from_the_file(&fixture, &window), case["asked"] == true, "{case}");
    }
}

#[test]
fn the_ring_plan_fixture_is_what_the_plugin_sends_and_reads() {
    let fixture = fixture("transfer-v1.ring-plan.fixture.json");
    let plan = &fixture["plan"];
    let input = &plan["input"];
    let params = plan_params(
        input["callId"].as_str().unwrap(),
        input["callEpoch"].as_u64().unwrap(),
        input["ownerEpoch"].as_u64().unwrap(),
        parse_arguments(&json!({"reason": input["reason"]})).unwrap(),
        input["callerNumber"].as_str(),
        &strings(&input["recentCallerTurns"]),
    );
    assert_eq!(params, plan["params"]);
    assert_eq!(plan["method"], "oaiy.ring.plan");
    assert_eq!(fixture["planWaitMillis"], 1500);

    for case in plan["results"].as_array().unwrap() {
        let parsed = parse_plan(&case["result"]).unwrap_or_else(|error| panic!("{case}: {error}"));
        let expected = &case["parsed"];
        let decision = match parsed.decision {
            Decision::Ring => "ring",
            Decision::MessageOnly => "message_only",
            Decision::Refused => "refused",
        };
        assert_eq!(expected["decision"], decision, "{case}");
        if decision == "ring" {
            assert_eq!(expected["ringSeconds"], parsed.ring_seconds, "{case}");
        } else {
            assert_eq!(expected["reason"], parsed.reason.as_str(), "{case}");
        }
        assert_eq!(strings(&expected["targets"]), parsed.targets(), "{case}");
        // The host vouching for a reason other than caller_asked (absent is false).
        assert_eq!(
            expected["reasonAllowed"].as_bool().unwrap_or(false),
            parsed.reason_allowed,
            "{case}"
        );
        // How the request is aimed.
        if decision == "ring" {
            let rule = match parsed.target_rule() {
                Targets::Only(_) => "only",
                Targets::Nobody => "nobody",
            };
            assert_eq!(expected["targetRule"], rule, "{case}");
        } else {
            assert!(expected.get("targetRule").is_none(), "{case}");
        }
    }
    for rule in ["only", "nobody"] {
        assert!(plan["targetRules"][rule].is_string(), "{rule} is described");
    }
    assert!(
        plan["targetRules"].get("any_live").is_none(),
        "a toast is not a target: there is no rule that opens the request to any device"
    );
    let rules_seen: std::collections::BTreeSet<_> = plan["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|case| case["parsed"]["targetRule"].as_str())
        .collect();
    assert_eq!(rules_seen.len(), 2, "a case for each way to aim a ring");
    for case in plan["unusableResults"].as_array().unwrap() {
        assert!(parse_plan(&case["result"]).is_err(), "{case}");
    }

    let opened = &fixture["opened"];
    let input = &opened["input"];
    assert_eq!(opened["method"], "oaiy.ring.opened");
    assert_eq!(
        opened_params(
            input["planId"].as_str().unwrap(),
            input["requestId"].as_str().unwrap(),
            input["callId"].as_str().unwrap(),
            input["callEpoch"].as_u64().unwrap(),
            input["ownerEpoch"].as_u64().unwrap(),
            input["expiresAt"].as_u64().unwrap(),
        ),
        *input
    );
}
