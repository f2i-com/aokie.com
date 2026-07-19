use aokie_protocol::v2::{
    parse_mobile_frame, parse_plugin_frame, AdmissionClaims, MobileIdleSyncFrame, MobileInbound,
    PluginClaimRejectedFrame, PluginInbound, PluginLeaseStatus, PluginLeaseStatusFrame,
    PluginOfferAcceptedFrame,
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

fn direction_validator(direction: &str) -> jsonschema::Validator {
    let mut schema: Value = serde_json::from_str(SCHEMA).expect("v2 schema is JSON");
    schema
        .as_object_mut()
        .expect("v2 schema is an object")
        .remove("anyOf");
    schema["$ref"] = json!(format!("#/$defs/{direction}"));
    jsonschema::validator_for(&schema).expect("v2 direction schema compiles")
}

fn definition_validator(definition: &str) -> jsonschema::Validator {
    let mut schema: Value = serde_json::from_str(SCHEMA).expect("v2 schema is JSON");
    schema
        .as_object_mut()
        .expect("v2 schema is an object")
        .remove("anyOf");
    schema["$ref"] = json!(format!("#/$defs/{definition}"));
    jsonschema::validator_for(&schema).expect("v2 definition schema compiles")
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

fn pending_mobile_offer() -> Value {
    json!({
        "offer": {
            "offerId": "offer_a",
            "opportunityId": "opportunity_a",
            "targetDeviceId": "device_a",
            "targetHolderKeyThumbprint": "mobile_thumbprint_a",
            "offeredMode": "takeover",
            "surface": "in_app",
            "appId": "app_coastal_auto",
            "callId": "call_42",
            "callEpoch": 7,
            "ownerEpoch": 3,
            "switchboardRevision": 11,
            "remoteRevision": 13,
            "requiredConsentPolicyId": "aokie_remote_access",
            "requiredConsentPolicyVersion": 3,
            "requiredGrants": ["state_read", "rtc_signal", "takeover"],
            "issuedAt": 1_800_000_000_u64,
            "expiresAt": 1_800_000_025_u64,
            "jti": "offer_jti_a"
        },
        "offerToken": "signed.offer.token"
    })
}

fn lease_claims(mode: &str, phase: &str) -> Value {
    let (tracks, fence) = match (mode, phase) {
        ("monitor", "active") => (json!(["pstn_in", "pstn_out"]), 0),
        ("consult", "prepared") => (json!(["consult_rx"]), 0),
        ("consult", "active") => (json!(["consult_rx", "consult_tx"]), 0),
        ("takeover", "prepared") => (json!(["pstn_in"]), 1),
        ("takeover", "active") => (json!(["pstn_in", "pstn_out"]), 1),
        _ => panic!("test requested an invalid lease shape"),
    };
    json!({
        "aud": "aokie-companion-media",
        "appId": "app_coastal_auto",
        "pluginId": "plugin_a",
        "deviceId": "device_a",
        "pluginKeyThumbprint": "plugin_thumbprint_a",
        "mobileKeyThumbprint": "mobile_thumbprint_a",
        "callId": "call_42",
        "callEpoch": 7,
        "ownerEpoch": 3,
        "mode": mode,
        "phase": phase,
        "tracks": tracks,
        "expiresAt": 1_800_000_060_u64,
        "leaseId": "lease_a",
        "jti": "lease_jti_a",
        "fence": fence,
        "sessionNonce": "mobile_nonce_a",
        "rtcSessionId": "rtc_a"
    })
}

fn plugin_lease_status(status: &str, mode: &str, phase: &str) -> Value {
    json!({
        "kind": "plugin_lease_status",
        "schemaVersion": 2,
        "appId": "app_coastal_auto",
        "deviceId": "device_a",
        "requestId": "request_a",
        "status": status,
        "leaseToken": "signed.lease.token",
        "lease": lease_claims(mode, phase)
    })
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
fn authoritative_snapshots_keep_socket_omission_and_accept_relay_offers() {
    let root = validator();
    let plugin_outbound = direction_validator("pluginToGateway");
    let socket_snapshot: Value = serde_json::from_str(PLUGIN_SNAPSHOT).unwrap();

    assert!(
        socket_snapshot["snapshot"]
            .get("pendingMobileOffers")
            .is_none(),
        "the canonical socket fixture must remain byte-compatible"
    );
    assert!(root.is_valid(&socket_snapshot));
    assert!(plugin_outbound.is_valid(&socket_snapshot));
    let socket_frame = match parse_plugin_frame(PLUGIN_SNAPSHOT).unwrap() {
        PluginInbound::Snapshot(frame) => frame,
        other => panic!("expected plugin snapshot, got {other:?}"),
    };
    assert!(socket_frame.snapshot.pending_mobile_offers.is_empty());
    assert!(
        serde_json::to_value(socket_frame).unwrap()["snapshot"]
            .get("pendingMobileOffers")
            .is_none(),
        "an empty authoritative offer list must be omitted on the socket carrier"
    );

    let mut relay_snapshot = socket_snapshot.clone();
    relay_snapshot["snapshot"]["pendingMobileOffers"] = json!([pending_mobile_offer()]);
    assert!(root.is_valid(&relay_snapshot));
    assert!(plugin_outbound.is_valid(&relay_snapshot));
    let relay_frame = match parse_plugin_frame(&relay_snapshot.to_string()).unwrap() {
        PluginInbound::Snapshot(frame) => frame,
        other => panic!("expected plugin snapshot, got {other:?}"),
    };
    assert_eq!(relay_frame.snapshot.pending_mobile_offers.len(), 1);

    let mut flooded = socket_snapshot;
    flooded["snapshot"]["pendingMobileOffers"] = Value::Array(vec![pending_mobile_offer(); 9]);
    assert!(!root.is_valid(&flooded));
    assert!(!plugin_outbound.is_valid(&flooded));
    assert!(parse_plugin_frame(&flooded.to_string()).is_err());
}

#[test]
fn transfer_mute_and_live_media_truth_are_directionally_conformant() {
    let mobile_outbound = direction_validator("mobileToGateway");
    let plugin_outbound = direction_validator("pluginToGateway");
    let gateway_to_mobile = direction_validator("gatewayToMobile");
    let gateway_to_plugin = direction_validator("gatewayToPlugin");
    let signed_pending_mobile_offer = definition_validator("signedPendingMobileOffer");

    let mobile_mute = json!({
        "kind":"microphone_mute", "schemaVersion":2, "appId":"app_a",
        "requestId":"mute_request_a", "idempotencyKey":"mute_idem_a",
        "leaseToken":"signed.lease.token", "rtcSessionId":"rtc_a",
        "callId":"call_a", "callEpoch":7, "ownerEpoch":4,
        "switchboardRevision":11, "remoteRevision":14, "fence":9,
        "muted":true
    });
    assert!(mobile_outbound.is_valid(&mobile_mute));
    assert!(!plugin_outbound.is_valid(&mobile_mute));
    assert!(matches!(
        parse_mobile_frame(&mobile_mute.to_string()).unwrap(),
        MobileInbound::MicrophoneMute(_)
    ));

    let plugin_mute = json!({
        "kind":"plugin_microphone_mute", "schemaVersion":2, "appId":"app_a",
        "deviceId":"device_a", "requestId":"mute_request_a",
        "leaseId":"lease_a", "leaseJti":"lease_jti_a", "rtcSessionId":"rtc_a",
        "callId":"call_a", "callEpoch":7, "ownerEpoch":4,
        "switchboardRevision":11, "remoteRevision":14, "fence":9,
        "muted":true
    });
    assert!(gateway_to_plugin.is_valid(&plugin_mute));
    assert!(!plugin_outbound.is_valid(&plugin_mute));
    assert!(matches!(
        parse_plugin_frame(&plugin_mute.to_string()).unwrap(),
        PluginInbound::MicrophoneMute(_)
    ));

    let mute_status = json!({
        "kind":"microphone_mute_status", "schemaVersion":2, "appId":"app_a",
        "deviceId":"device_a", "requestId":"mute_request_a",
        "leaseId":"lease_a", "leaseJti":"lease_jti_a", "rtcSessionId":"rtc_a",
        "callId":"call_a", "callEpoch":7, "ownerEpoch":4,
        "switchboardRevision":11, "remoteRevision":15, "fence":9,
        "muted":true
    });
    assert!(plugin_outbound.is_valid(&mute_status));
    assert!(gateway_to_mobile.is_valid(&mute_status));
    assert!(matches!(
        parse_plugin_frame(&mute_status.to_string()).unwrap(),
        PluginInbound::MicrophoneMuteStatus(_)
    ));

    let mut transfer_offer = pending_mobile_offer();
    transfer_offer["offer"]["acceptedTransferRequestId"] = json!("assist_transfer_a");
    transfer_offer["offer"]["requiredGrants"] = json!([
        "state_read",
        "rtc_signal",
        "takeover",
        "resume_aokie",
        "assistance_respond"
    ]);
    assert!(signed_pending_mobile_offer.is_valid(&transfer_offer));
    let mut unsafe_offer = transfer_offer.clone();
    unsafe_offer["offer"]["requiredGrants"] =
        json!(["state_read", "rtc_signal", "takeover", "resume_aokie"]);
    assert!(!signed_pending_mobile_offer.is_valid(&unsafe_offer));

    let mut transfer_lease: Value = serde_json::from_str(LEASE_REQUEST).unwrap();
    transfer_lease["acceptedTransferRequestId"] = json!("assist_transfer_a");
    assert!(mobile_outbound.is_valid(&transfer_lease));
    assert!(matches!(
        parse_mobile_frame(&transfer_lease.to_string()).unwrap(),
        MobileInbound::LeaseRequest(_)
    ));
    transfer_lease["mode"] = json!("consult");
    assert!(!mobile_outbound.is_valid(&transfer_lease));
    assert!(parse_mobile_frame(&transfer_lease.to_string()).is_err());

    let mut transfer_request: Value = serde_json::from_str(ASSISTANCE_REQUEST).unwrap();
    transfer_request["transferOffered"] = json!(true);
    assert!(plugin_outbound.is_valid(&transfer_request));
    let mut decline: Value = serde_json::from_str(ASSISTANCE_ANSWER).unwrap();
    decline["responseAction"] = json!("decline");
    decline["answer"] = json!("declined");
    assert!(mobile_outbound.is_valid(&decline));
    assert!(matches!(
        parse_mobile_frame(&decline.to_string()).unwrap(),
        MobileInbound::AssistanceAnswer(_)
    ));

    let mut live_snapshot: Value = serde_json::from_str(PLUGIN_SNAPSHOT).unwrap();
    live_snapshot["snapshot"]["serviceMode"] = json!("human_active");
    live_snapshot["snapshot"]["participants"] = json!([{
        "participantId":"rtc_a", "mode":"talker", "state":"active",
        "subjectId":"device_a", "displayLabel":"Owner Companion"
    }]);
    live_snapshot["snapshot"]["audioLevels"] = json!([
        {"source":"caller", "levelPermille":420},
        {"source":"companion", "participantId":"rtc_a", "levelPermille":730}
    ]);
    live_snapshot["snapshot"]["companionMicrophoneMuted"] = json!(true);
    assert!(plugin_outbound.is_valid(&live_snapshot));
    assert!(matches!(
        parse_plugin_frame(&live_snapshot.to_string()).unwrap(),
        PluginInbound::Snapshot(_)
    ));
}

#[test]
fn relay_authority_frames_are_valid_plugin_outbound_contracts() {
    let root = validator();
    let plugin_outbound = direction_validator("pluginToGateway");
    let mobile_outbound = direction_validator("mobileToGateway");
    let gateway_to_plugin = direction_validator("gatewayToPlugin");

    let accepted = json!({
        "kind": "plugin_offer_accepted",
        "schemaVersion": 2,
        "appId": "app_coastal_auto",
        "deviceId": "device_a",
        "requestId": "request_a",
        "offerId": "offer_a",
        "offerJti": "offer_jti_a",
        "offeredMode": "takeover",
        "accepted": true
    });
    assert!(root.is_valid(&accepted));
    assert!(plugin_outbound.is_valid(&accepted));
    assert!(!mobile_outbound.is_valid(&accepted));
    assert!(!gateway_to_plugin.is_valid(&accepted));
    serde_json::from_value::<PluginOfferAcceptedFrame>(accepted)
        .unwrap()
        .validate()
        .unwrap();

    let rejected = json!({
        "kind": "plugin_claim_rejected",
        "schemaVersion": 2,
        "appId": "app_coastal_auto",
        "deviceId": "device_a",
        "requestId": "request_a",
        "code": "consent_required",
        "message": "Remote takeover consent is not current"
    });
    assert!(root.is_valid(&rejected));
    assert!(plugin_outbound.is_valid(&rejected));
    assert!(!mobile_outbound.is_valid(&rejected));
    assert!(!gateway_to_plugin.is_valid(&rejected));
    serde_json::from_value::<PluginClaimRejectedFrame>(rejected)
        .unwrap()
        .validate()
        .unwrap();

    for (status, mode, phase, expected) in [
        ("granted", "monitor", "active", PluginLeaseStatus::Granted),
        (
            "provisional",
            "takeover",
            "prepared",
            PluginLeaseStatus::Provisional,
        ),
        ("active", "takeover", "active", PluginLeaseStatus::Active),
        ("renewed", "takeover", "active", PluginLeaseStatus::Renewed),
    ] {
        let value = plugin_lease_status(status, mode, phase);
        assert!(root.is_valid(&value), "root rejected {status}: {value:#}");
        assert!(
            plugin_outbound.is_valid(&value),
            "plugin direction rejected {status}: {value:#}"
        );
        assert!(!mobile_outbound.is_valid(&value));
        assert!(!gateway_to_plugin.is_valid(&value));
        let frame: PluginLeaseStatusFrame = serde_json::from_value(value).unwrap();
        assert_eq!(frame.status, expected);
        frame.validate(1_800_000_000).unwrap();
    }
}

#[test]
fn relay_authority_schema_rejects_ambiguous_or_unsafe_frames() {
    let root = validator();
    let plugin_outbound = direction_validator("pluginToGateway");

    let mut refusal_disguised_as_acceptance = json!({
        "kind": "plugin_offer_accepted",
        "schemaVersion": 2,
        "appId": "app_coastal_auto",
        "deviceId": "device_a",
        "requestId": "request_a",
        "offerId": "offer_a",
        "offerJti": "offer_jti_a",
        "offeredMode": "takeover",
        "accepted": true
    });
    refusal_disguised_as_acceptance["accepted"] = json!(false);
    assert!(!root.is_valid(&refusal_disguised_as_acceptance));
    assert!(!plugin_outbound.is_valid(&refusal_disguised_as_acceptance));
    let frame: PluginOfferAcceptedFrame =
        serde_json::from_value(refusal_disguised_as_acceptance).unwrap();
    assert!(frame.validate().is_err());

    let unknown_status = plugin_lease_status("prepared", "takeover", "prepared");
    assert!(!root.is_valid(&unknown_status));
    assert!(!plugin_outbound.is_valid(&unknown_status));
    assert!(serde_json::from_value::<PluginLeaseStatusFrame>(unknown_status).is_err());

    let contradictory_status = plugin_lease_status("granted", "takeover", "prepared");
    assert!(!root.is_valid(&contradictory_status));
    assert!(!plugin_outbound.is_valid(&contradictory_status));
    let frame: PluginLeaseStatusFrame = serde_json::from_value(contradictory_status).unwrap();
    assert!(frame.validate(1_800_000_000).is_err());

    let mut empty_rejection = json!({
        "kind": "plugin_claim_rejected",
        "schemaVersion": 2,
        "appId": "app_coastal_auto",
        "deviceId": "device_a",
        "requestId": "request_a",
        "code": "consent_required",
        "message": "Remote takeover consent is not current"
    });
    empty_rejection["message"] = json!("");
    assert!(!root.is_valid(&empty_rejection));
    assert!(!plugin_outbound.is_valid(&empty_rejection));
    let frame: PluginClaimRejectedFrame = serde_json::from_value(empty_rejection).unwrap();
    assert!(frame.validate().is_err());

    let mut unaddressed = plugin_lease_status("active", "takeover", "active");
    unaddressed.as_object_mut().unwrap().remove("deviceId");
    assert!(!root.is_valid(&unaddressed));
    assert!(!plugin_outbound.is_valid(&unaddressed));
    assert!(serde_json::from_value::<PluginLeaseStatusFrame>(unaddressed).is_err());
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
