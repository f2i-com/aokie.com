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

const FIXTURES: [&str; 7] = [
    "transfer-v1.tool-call.fixture.json",
    "transfer-v1.tool-result.fixture.json",
    "transfer-v1.outcome.fixture.json",
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

    // SHA256SUMS lists every fixture (the shared, byte-identical set; the
    // prose contract is each repository's own), and every digest is current.
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

#[test]
fn the_caller_asked_fixture_passes_and_its_rules_are_the_plugins_rules() {
    let fixture = fixture("transfer-v1.caller-asked.fixture.json");
    assert_eq!(fixture["recentTurns"], 3);
    let turns = |case: &Value| strings(&case["turns"]);
    assert_eq!(fixture["positive"].as_array().unwrap().len(), 8);
    assert_eq!(fixture["negative"].as_array().unwrap().len(), 8);
    for case in fixture["positive"].as_array().unwrap() {
        assert!(caller_asked(&turns(case)), "{case}");
    }
    for case in fixture["negative"].as_array().unwrap() {
        assert!(!caller_asked(&turns(case)), "{case}");
    }
    for case in fixture["window"].as_array().unwrap() {
        assert_eq!(caller_asked(&turns(case)), case["asked"] == true, "{case}");
    }
    for case in fixture["knownGaps"]["cases"].as_array().unwrap() {
        assert_eq!(caller_asked(&turns(case)), case["asked"] == true, "{case}");
    }

    // The rules the file spells out, compiled here, decide every case the
    // way the plugin's own compiled rules do.
    let person = fixture["person"].as_str().unwrap();
    let compile = |source: &Value| {
        regex::Regex::new(&source.as_str().unwrap().replace("<person>", person)).unwrap()
    };
    let rules: Vec<_> = fixture["rules"].as_array().unwrap().iter().map(compile).collect();
    let blocks: Vec<_> = fixture["blocks"].as_array().unwrap().iter().map(compile).collect();
    assert_eq!((rules.len(), blocks.len()), (7, 6));
    let with_fixture_rules = |turns: &[String]| {
        turns[turns.len().saturating_sub(3)..].iter().any(|turn| {
            let turn = phrase::normalize(turn);
            !blocks.iter().any(|block| block.is_match(&turn))
                && rules.iter().any(|rule| rule.is_match(&turn))
        })
    };
    let every_case = ["positive", "negative"]
        .iter()
        .flat_map(|key| fixture[key].as_array().unwrap().iter())
        .chain(fixture["window"].as_array().unwrap().iter())
        .chain(fixture["knownGaps"]["cases"].as_array().unwrap().iter());
    for case in every_case {
        let turns = turns(case);
        assert_eq!(with_fixture_rules(&turns), caller_asked(&turns), "{case}");
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
        // How the request is aimed.
        if decision == "ring" {
            let rule = match parsed.target_rule() {
                Targets::Only(_) => "only",
                Targets::AnyLive => "any_live",
                Targets::Nobody => "nobody",
            };
            assert_eq!(expected["targetRule"], rule, "{case}");
        } else {
            assert!(expected.get("targetRule").is_none(), "{case}");
        }
    }
    for rule in ["only", "any_live", "nobody"] {
        assert!(plan["targetRules"][rule].is_string(), "{rule} is described");
    }
    let rules_seen: std::collections::BTreeSet<_> = plan["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|case| case["parsed"]["targetRule"].as_str())
        .collect();
    assert_eq!(rules_seen.len(), 3, "a case for each way to aim a ring");
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
