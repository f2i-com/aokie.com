use aokie_protocol::{CallSnapshot, SyncReadyFrame, MAX_JSON_SAFE_INTEGER};
use serde_json::{json, Value};

const CONTRACT_SCHEMA: &str =
    include_str!("../../../docs/contracts/aokie-companion-realtime.v1.schema.json");
const SYNC_READY_FIXTURE: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-sync-ready.v1.json");
const CALL_SNAPSHOT_FIXTURE: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-live-snapshot.v1.json");

fn validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(CONTRACT_SCHEMA).expect("contract schema is JSON");
    jsonschema::validator_for(&schema).expect("contract schema compiles")
}

fn assert_schema_valid(instance: &Value) {
    let validator = validator();
    let errors: Vec<_> = validator
        .iter_errors(instance)
        .map(|error| format!("{error} at {}", error.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "instance failed contract validation:\n{}",
        errors.join("\n")
    );
}

fn rust_rejects(instance: &Value) -> bool {
    serde_json::from_value::<SyncReadyFrame>(instance.clone())
        .map(|frame| frame.validate().is_err())
        .unwrap_or(true)
}

#[test]
fn canonical_sync_ready_fixture_conforms_and_round_trips() {
    let instance: Value = serde_json::from_str(SYNC_READY_FIXTURE).expect("fixture is JSON");
    assert_schema_valid(&instance);

    let frame: SyncReadyFrame =
        serde_json::from_value(instance.clone()).expect("fixture matches the Rust wire model");
    frame.validate().expect("fixture satisfies Rust invariants");
    assert_eq!(frame.kind, "sync_ready");
    assert_eq!(frame.app_id, "app_coastal_auto");
    assert_eq!(
        serde_json::to_value(frame).expect("frame serializes"),
        instance
    );
}

#[test]
fn sync_ready_schema_and_rust_model_reject_shape_and_semantic_drift() {
    let valid: Value = serde_json::from_str(SYNC_READY_FIXTURE).expect("fixture is JSON");
    let mut invalid = Vec::new();

    let mut missing_app = valid.clone();
    missing_app
        .as_object_mut()
        .expect("fixture object")
        .remove("appId");
    invalid.push(missing_app);

    let mut extra = valid.clone();
    extra["unexpected"] = json!(true);
    invalid.push(extra);

    for (field, value) in [
        ("kind", json!("ready")),
        ("schemaVersion", json!(2)),
        ("appId", json!("wrong app")),
        ("streamNonce", json!("unsafe nonce")),
        ("sequence", json!(MAX_JSON_SAFE_INTEGER + 1)),
        ("sequence", json!("42")),
    ] {
        let mut instance = valid.clone();
        instance[field] = value;
        invalid.push(instance);
    }

    let validator = validator();
    for instance in invalid {
        assert!(
            !validator.is_valid(&instance),
            "schema accepted invalid sync_ready: {instance:#}"
        );
        assert!(
            rust_rejects(&instance),
            "Rust model accepted invalid sync_ready: {instance:#}"
        );
    }
}

#[test]
fn live_caption_permission_is_fail_closed_in_schema_and_rust() {
    let valid: Value = serde_json::from_str(CALL_SNAPSHOT_FIXTURE).expect("fixture is JSON");
    assert_schema_valid(&valid);

    let mut denied_with_captions = valid.clone();
    denied_with_captions["capabilities"]["liveCaptions"] = json!(false);
    assert!(
        !validator().is_valid(&denied_with_captions),
        "schema allowed captions without the live-caption capability"
    );
    let denied: CallSnapshot = serde_json::from_value(denied_with_captions)
        .expect("the denied fixture still matches the snapshot shape");
    assert!(denied.validate().is_err());

    let mut denied_without_captions = valid;
    denied_without_captions["capabilities"]["liveCaptions"] = json!(false);
    denied_without_captions["captions"] = json!([]);
    assert_schema_valid(&denied_without_captions);
    let denied: CallSnapshot = serde_json::from_value(denied_without_captions)
        .expect("caption-disabled snapshot matches the Rust model");
    denied
        .validate()
        .expect("caption-disabled snapshot is safe when captions are empty");
}
