//! Contract tests: the shipped manifest must validate against the
//! local copy of `plugin-manifest.schema.json`, and every event the
//! plugin emits (the dev-mode scripted lifecycle exercises the whole
//! `aokie.*` mock surface) must validate against
//! `desktop-event.schema.json`. Schemas live in `docs/contracts/`
//! (canonical copies: formlogic-app repo).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use aokie_plugin::connector::Plugin;
use aokie_plugin::event_bridge::VecSink;
use serde_json::{json, Value};

fn contracts_dir() -> PathBuf {
    // crates/aokie-plugin → repo root → docs/contracts
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/contracts")
}

fn plugin_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn load_json(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn validator_for(schema_file: &str) -> jsonschema::Validator {
    let schema = load_json(&contracts_dir().join(schema_file));
    jsonschema::validator_for(&schema)
        .unwrap_or_else(|e| panic!("{schema_file} is not a valid schema: {e}"))
}

fn assert_valid(validator: &jsonschema::Validator, instance: &Value, what: &str) {
    if !validator.is_valid(instance) {
        let errors: Vec<String> = validator
            .iter_errors(instance)
            .map(|e| format!("{} at {}", e, e.instance_path))
            .collect();
        panic!(
            "{what} failed schema validation:\n{}\ninstance: {instance:#}",
            errors.join("\n")
        );
    }
}

#[test]
fn manifest_validates_against_plugin_manifest_schema() {
    let _guard = plugin_test_lock().lock().expect("plugin test lock");
    let manifest = load_json(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest.json"));
    let validator = validator_for("plugin-manifest.schema.json");
    assert_valid(&validator, &manifest, "manifest.json");

    // Contract identity pins (AOKIE_PLUGIN_CONTRACT.md §1).
    assert_eq!(manifest["id"], json!("aokie"));
    assert_eq!(manifest["pluginApiVersion"], json!(1));
    assert_eq!(manifest["entry"]["kind"], json!("process"));

    // Every command the dispatcher implements must be declared, and
    // vice versa Desktop rejects undeclared commands before they
    // reach us — keep the two lists identical.
    let declared: Vec<&str> = manifest["connectors"][0]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    let mut plugin = Plugin::ephemeral(true);
    let mut sink = VecSink::default();
    for command in &declared {
        // Probe the dispatcher with a deliberately invalid field. Every real
        // handler rejects it during shape validation, before hardware,
        // consent, pairing, or background-worker side effects can start.
        // The unknown-command branch remains distinguishable below.
        let result = plugin.dispatch_command(command, &json!({"__contractProbe": true}), &mut sink);
        // "unknown command" is the only unacceptable outcome — typed
        // command_failed for unwired hardware is contract-legal.
        if let Err(e) = result {
            assert!(
                !e.message.contains("unknown command"),
                "declared command {command} is not implemented: {}",
                e.message
            );
        }
    }

    // Every connector command maps 1:1 onto a `connector.aokie.*`
    // capability. Host-facing capabilities (e.g. `flow.run`, which lets
    // the plugin request a FormLogic Flow run) are declared alongside and
    // are NOT connector commands — allow them past the 1:1 check.
    let capabilities: Vec<&str> = manifest["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for command in &declared {
        let cap = format!("connector.aokie.{command}");
        assert!(
            capabilities.contains(&cap.as_str()),
            "command {command} has no matching capability {cap}"
        );
    }
    // Reverse: no `connector.aokie.*` capability without a declared command.
    let connector_caps: Vec<&&str> = capabilities
        .iter()
        .filter(|c| c.starts_with("connector.aokie."))
        .collect();
    for cap in &connector_caps {
        let command = cap.strip_prefix("connector.aokie.").unwrap();
        assert!(
            declared.contains(&command),
            "capability {cap} has no matching declared command"
        );
    }
    assert_eq!(connector_caps.len(), declared.len());
}

#[test]
fn mock_lifecycle_events_validate_against_desktop_event_schema() {
    let _guard = plugin_test_lock().lock().expect("plugin test lock");
    let validator = validator_for("desktop-event.schema.json");
    let mut plugin = Plugin::ephemeral(true);
    let mut sink = VecSink::default();
    plugin
        .dispatch_command(
            "dongle.diagnostics",
            &json!({"simulate": "call"}),
            &mut sink,
        )
        .expect("simulated call runs in dev mode");

    // Exercise the manually-driven emitters too.
    plugin
        .dispatch_command(
            "sms.send",
            &json!({"to": "+61432123456", "body": "confirmed"}),
            &mut sink,
        )
        .unwrap();
    plugin
        .dispatch_command("phone.startPairing", &Value::Null, &mut sink)
        .unwrap();

    let manifest = load_json(&Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest.json"));
    let declared_events: Vec<&str> = manifest["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();

    let mut seen = 0;
    for line in &sink.lines {
        let msg: Value = serde_json::from_str(line).unwrap();
        if msg["method"] != json!("event.emit") {
            continue;
        }
        let event = &msg["params"]["event"];
        assert_valid(&validator, event, &format!("event {}", event["name"]));
        // Desktop drops events not declared in the manifest — every
        // emission must be declared or it silently disappears.
        let name = event["name"].as_str().unwrap();
        assert!(
            declared_events.contains(&name),
            "emitted event {name} is not declared in manifest.json"
        );
        assert_eq!(event["source"], json!("aokie"));
        seen += 1;
    }
    // 8 scripted + sms.sent + phone.pairing_started
    assert_eq!(seen, 10);
}
