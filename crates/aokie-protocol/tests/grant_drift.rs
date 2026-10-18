//! Drift tests: every copy of the protocol-v2 grant list outside this crate
//! must name exactly the grants `Grant::ALL` defines. A copy that falls behind
//! fails quietly in the field: the self-host minter refused the newer grants
//! (so self-hosters could not grant End caller, participants or audio
//! levels), and the phone's TypeScript bridge refuses any grant it does not
//! list.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use aokie_protocol::v2::Grant;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn rust_grants() -> BTreeSet<String> {
    Grant::ALL
        .iter()
        .map(|grant| grant.as_str().to_owned())
        .collect()
}

/// The double-quoted strings between `start` and the first `end` after it.
fn quoted_between(source: &str, start: &str, end: &str, what: &str) -> BTreeSet<String> {
    let from = source
        .find(start)
        .unwrap_or_else(|| panic!("{what}: `{start}` not found"))
        + start.len();
    let body = &source[from..];
    let to = body
        .find(end)
        .unwrap_or_else(|| panic!("{what}: no `{end}` after `{start}`"));
    let body = &body[..to];
    let mut names = BTreeSet::new();
    let mut rest = body;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let close = after
            .find('"')
            .unwrap_or_else(|| panic!("{what}: unterminated string"));
        assert!(
            names.insert(after[..close].to_owned()),
            "{what} lists {} twice",
            &after[..close]
        );
        rest = &after[close + 1..];
    }
    assert!(!names.is_empty(), "{what}: no names found");
    names
}

#[test]
fn the_v2_json_schema_grant_enum_matches_the_protocol() {
    let schema: serde_json::Value = serde_json::from_str(&read(
        "docs/contracts/aokie-companion-realtime.v2.schema.json",
    ))
    .expect("v2 schema parses");
    let listed: BTreeSet<String> = schema["$defs"]["grant"]["enum"]
        .as_array()
        .expect("$defs.grant.enum")
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(listed, rust_grants());
}

#[test]
fn the_self_host_credential_minter_knows_every_grant() {
    let source = read("deploy/companion-self-host/examples/mint_credentials.py");
    let listed = quoted_between(&source, "SCOPES = {", "}", "mint_credentials.py SCOPES");
    assert_eq!(
        listed,
        rust_grants(),
        "deploy/companion-self-host/examples/mint_credentials.py SCOPES drifted from Grant::ALL"
    );
}

#[test]
fn the_phone_bridge_knows_every_grant() {
    let bridge = read("apps/aokie-mobile/src/bridge/tauriBridge.ts");
    let listed = quoted_between(
        &bridge,
        "const V2_GRANTS = new Set<V2Grant>([",
        "]",
        "tauriBridge.ts V2_GRANTS",
    );
    assert_eq!(listed, rust_grants(), "tauriBridge.ts V2_GRANTS drifted from Grant::ALL");

    let types = read("apps/aokie-mobile/src/bridge/CompanionBridge.ts");
    let union = quoted_between(
        &types,
        "export type V2Grant =",
        ";",
        "CompanionBridge.ts V2Grant",
    );
    assert_eq!(union, rust_grants(), "CompanionBridge.ts V2Grant drifted from Grant::ALL");
}
