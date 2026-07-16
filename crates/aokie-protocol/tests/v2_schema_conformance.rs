use aokie_protocol::v2::{
    parse_mobile_frame, parse_plugin_frame, AdmissionClaims, MobileIdleSyncFrame, MobileInbound,
    PluginInbound,
};
use serde_json::{json, Value};

const SCHEMA: &str =
    include_str!("../../../docs/contracts/aokie-companion-realtime.v2.schema.json");
const MOBILE_HELLO: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-mobile-hello.v2.json");
const PLUGIN_SNAPSHOT: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-plugin-snapshot.v2.json");
const LEASE_REQUEST: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-takeover-lease-request.v2.json");
const RTC_OFFER: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-rtc-offer.v2.json");
const ADMISSION: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-admission-claims.v2.json");
const ASSISTANCE_REQUEST: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-assistance-request.v2.json");
const ASSISTANCE_ANSWER: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-assistance-answer.v2.json");
const END_CALLER_CHALLENGE_REQUEST: &str = include_str!(
    "../../../docs/contracts/fixtures/aokie-companion-end-caller-challenge-request.v2.json"
);
const END_CALLER_CONFIRM: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-end-caller-confirm.v2.json");
const END_CALLER_EXECUTE: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-end-caller-execute.v2.json");
const END_CALLER_RESULT: &str =
    include_str!("../../../docs/contracts/fixtures/aokie-companion-end-caller-result.v2.json");

fn validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(SCHEMA).expect("v2 schema is JSON");
    jsonschema::validator_for(&schema).expect("v2 schema compiles")
}

fn assert_valid(encoded: &str) -> Value {
    let value: Value = serde_json::from_str(encoded).expect("fixture is JSON");
    let errors: Vec<_> = validator()
        .iter_errors(&value)
        .map(|error| format!("{error} at {}", error.instance_path))
        .collect();
    assert!(
        errors.is_empty(),
        "fixture failed v2 schema:\n{}",
        errors.join("\n")
    );
    value
}

#[test]
fn canonical_directional_fixtures_match_schema_and_rust_models() {
    assert_valid(MOBILE_HELLO);
    assert!(matches!(
        parse_mobile_frame(MOBILE_HELLO).unwrap(),
        MobileInbound::Hello(_)
    ));

    assert_valid(PLUGIN_SNAPSHOT);
    assert!(matches!(
        parse_plugin_frame(PLUGIN_SNAPSHOT).unwrap(),
        PluginInbound::Snapshot(_)
    ));

    let plugin_idle = json!({
        "kind":"plugin_idle", "schemaVersion":2,
        "appId":"app_a", "eventId":"idle_a"
    });
    assert_valid(&plugin_idle.to_string());
    assert!(matches!(
        parse_plugin_frame(&plugin_idle.to_string()).unwrap(),
        PluginInbound::Idle(_)
    ));

    let idle_sync = json!({
        "kind":"idle_sync", "schemaVersion":2, "appId":"app_a",
        "sequence":1, "grants":["state_read", "monitor"]
    });
    assert_valid(&idle_sync.to_string());
    let idle: MobileIdleSyncFrame = serde_json::from_value(idle_sync).unwrap();
    idle.validate().unwrap();

    assert_valid(LEASE_REQUEST);
    assert!(matches!(
        parse_mobile_frame(LEASE_REQUEST).unwrap(),
        MobileInbound::LeaseRequest(_)
    ));

    assert_valid(RTC_OFFER);
    assert!(matches!(
        parse_mobile_frame(RTC_OFFER).unwrap(),
        MobileInbound::RtcSignal(_)
    ));

    let admission = assert_valid(ADMISSION);
    let claims: AdmissionClaims = serde_json::from_value(admission).unwrap();
    claims.validate(1_784_160_000).unwrap();

    assert_valid(ASSISTANCE_REQUEST);
    assert!(matches!(
        parse_plugin_frame(ASSISTANCE_REQUEST).unwrap(),
        PluginInbound::AssistanceRequest(_)
    ));

    assert_valid(ASSISTANCE_ANSWER);
    assert!(matches!(
        parse_mobile_frame(ASSISTANCE_ANSWER).unwrap(),
        MobileInbound::AssistanceAnswer(_)
    ));

    assert_valid(END_CALLER_CHALLENGE_REQUEST);
    assert!(matches!(
        parse_mobile_frame(END_CALLER_CHALLENGE_REQUEST).unwrap(),
        MobileInbound::EndCallerChallengeRequest(_)
    ));

    assert_valid(END_CALLER_CONFIRM);
    assert!(matches!(
        parse_mobile_frame(END_CALLER_CONFIRM).unwrap(),
        MobileInbound::EndCallerConfirm(_)
    ));

    assert_valid(END_CALLER_EXECUTE);

    assert_valid(END_CALLER_RESULT);
    assert!(matches!(
        parse_plugin_frame(END_CALLER_RESULT).unwrap(),
        PluginInbound::EndCallerResult(_)
    ));
}

#[test]
fn schema_and_direction_parser_reject_shape_confusion() {
    let mut mobile: Value = serde_json::from_str(LEASE_REQUEST).unwrap();
    mobile["admin"] = json!(true);
    assert!(!validator().is_valid(&mobile));
    assert!(parse_mobile_frame(&mobile.to_string()).is_err());

    assert!(parse_plugin_frame(LEASE_REQUEST).is_err());
    assert!(parse_mobile_frame(PLUGIN_SNAPSHOT).is_err());

    let invented_idle_call = json!({
        "kind":"plugin_idle", "schemaVersion":2,
        "appId":"app_a", "eventId":"idle_a", "callId":"call_a"
    });
    assert!(!validator().is_valid(&invented_idle_call));
    assert!(parse_plugin_frame(&invented_idle_call.to_string()).is_err());

    let unsafe_idle_sync = json!({
        "kind":"idle_sync", "schemaVersion":2, "appId":"app_a",
        "sequence":0, "grants":["monitor"]
    });
    assert!(!validator().is_valid(&unsafe_idle_sync));
    let unsafe_idle: MobileIdleSyncFrame = serde_json::from_value(unsafe_idle_sync).unwrap();
    assert!(unsafe_idle.validate().is_err());

    let mut rtc: Value = serde_json::from_str(RTC_OFFER).unwrap();
    rtc["signal"]["pcm"] = json!([1, 2, 3]);
    assert!(!validator().is_valid(&rtc));
    assert!(parse_mobile_frame(&rtc.to_string()).is_err());

    let offer_answer = json!({
        "kind":"mobile_offer_answer", "schemaVersion":2, "appId":"app_a",
        "requestId":"answer_a", "idempotencyKey":"answer-key-a",
        "offerId":"offer_a", "offerJti":"offer_jti_a", "offerToken":"offer_token_a",
        "targetDeviceId":"device_a", "targetHolderKeyThumbprint":"mobile_thumbprint_a",
        "offeredMode":"takeover", "callId":"call_a", "callEpoch":7, "ownerEpoch":3
    });
    assert!(validator().is_valid(&offer_answer));
    assert!(matches!(
        parse_mobile_frame(&offer_answer.to_string()).unwrap(),
        MobileInbound::OfferAnswer(_)
    ));
    let mut missing_mode = offer_answer;
    missing_mode.as_object_mut().unwrap().remove("offeredMode");
    assert!(!validator().is_valid(&missing_mode));
    assert!(parse_mobile_frame(&missing_mode.to_string()).is_err());

    let mut unsupported_consult: Value = serde_json::from_str(PLUGIN_SNAPSHOT).unwrap();
    unsupported_consult["snapshot"]["remoteCapabilities"]["softwareHold"] = json!(false);
    assert!(!validator().is_valid(&unsupported_consult));
    assert!(parse_plugin_frame(&unsupported_consult.to_string()).is_err());

    let mut unobserved_secondary: Value = serde_json::from_str(PLUGIN_SNAPSHOT).unwrap();
    unobserved_secondary["snapshot"]["secondaryCall"] = json!({
        "stable":false, "callbackEligible":false, "status":"queued"
    });
    assert!(!validator().is_valid(&unobserved_secondary));
    assert!(parse_plugin_frame(&unobserved_secondary.to_string()).is_err());

    let mut impossible_audio_level: Value = serde_json::from_str(PLUGIN_SNAPSHOT).unwrap();
    impossible_audio_level["snapshot"]["audioLevels"] =
        json!([{ "source":"caller", "levelPermille":1001 }]);
    assert!(!validator().is_valid(&impossible_audio_level));
    assert!(parse_plugin_frame(&impossible_audio_level.to_string()).is_err());
}
