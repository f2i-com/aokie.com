import { describe, expect, it } from "vitest";
import {
  LOCAL_MEDIA_PROOF_MAX_TTL_MS,
  parseCompanionAvailability,
  parseCompanionBootstrap,
  parseCompanionCallRecordDetail,
  parseCompanionCallRecords,
  parseCompanionHistory,
  parseCustomServerAuthorization,
  parseDesktopPairingDecision,
  parseDesktopPairingReview,
  parseDesktopPeerTrustChallenge,
  parseManagedAdmissionState,
  parseManagedConnectConfig,
  parseLocalMediaProof,
  parseNativeMediaSession,
  parseNativeMediaLevelsEvent,
  parseNativeMediaSignalEvent,
  parseNativeAudioDevices,
  parseServerProfiles,
  parseSyncReady,
  parseV2EndCaller,
  parseV2AssistanceAnswerAccepted,
  parseV2IdleSync,
  parseV2Snapshot,
} from "./tauriBridge";

const PROOF_RECEIVED_AT = Date.parse("2026-07-16T00:00:00Z");
const PEER_TRUST_RECEIVED_AT = Math.floor(PROOF_RECEIVED_AT / 1_000);

function localMediaProof(expiresAt: string) {
  return {
    appId: "app_test",
    streamNonce: "stream_test",
    rtcSessionId: "rtc_test",
    callId: "call_test",
    callEpoch: 3,
    ownerEpoch: 8,
    deviceId: "device_test",
    leaseId: "lease_test",
    mode: "talk",
    fence: 2,
    sdpRevision: 1,
    transportGeneration: 1,
    expiresAt,
    active: true,
  };
}

describe("local native media proof", () => {
  it("accepts only a complete lease-bound proof", () => {
    expect(parseLocalMediaProof(
      localMediaProof("2026-07-16T00:00:05Z"),
      PROOF_RECEIVED_AT,
    )).toMatchObject({ leaseId: "lease_test", active: true });
    expect(() => parseLocalMediaProof({
      streamNonce: "stream_test",
      callId: "call_test",
      callEpoch: 3,
      ownerEpoch: 8,
      deviceId: "device_test",
      leaseId: "",
      fence: 0,
      expiresAt: "2026-07-16T00:00:05Z",
      active: true,
    }, PROOF_RECEIVED_AT)).toThrow("Invalid local media proof");
  });

  it("requires short receipt-time proof heartbeats", () => {
    const maximumExpiry = new Date(PROOF_RECEIVED_AT + LOCAL_MEDIA_PROOF_MAX_TTL_MS).toISOString();
    const excessiveExpiry = new Date(PROOF_RECEIVED_AT + LOCAL_MEDIA_PROOF_MAX_TTL_MS + 1).toISOString();

    expect(parseLocalMediaProof(localMediaProof(maximumExpiry), PROOF_RECEIVED_AT)).not.toBeNull();
    expect(() => parseLocalMediaProof(localMediaProof(excessiveExpiry), PROOF_RECEIVED_AT))
      .toThrow("Invalid local media proof");
    expect(() => parseLocalMediaProof(localMediaProof("2026-07-16T00:00:00Z"), PROOF_RECEIVED_AT))
      .toThrow("Invalid local media proof");
  });

  it("binds proof mode to its exact fence semantics", () => {
    const talk = localMediaProof("2026-07-16T00:00:05Z");
    expect(parseLocalMediaProof(talk, PROOF_RECEIVED_AT)).toMatchObject({ mode: "talk", fence: 2 });
    expect(parseLocalMediaProof(
      { ...talk, mode: "consult", fence: 0 },
      PROOF_RECEIVED_AT,
    )).toMatchObject({ mode: "consult", fence: 0 });
    expect(() => parseLocalMediaProof(
      { ...talk, mode: "consult", fence: 1 },
      PROOF_RECEIVED_AT,
    )).toThrow("Invalid local media proof");
    expect(() => parseLocalMediaProof(
      { ...talk, mode: "talk", fence: 0 },
      PROOF_RECEIVED_AT,
    )).toThrow("Invalid local media proof");
  });

  it("allows an explicit proof reset", () => {
    expect(parseLocalMediaProof(null)).toBeNull();
  });

  it("validates idle authoritative sync proofs", () => {
    expect(parseSyncReady({ streamNonce: "stream_idle", sequence: 0 })).toEqual({
      streamNonce: "stream_idle",
      sequence: 0,
    });
    expect(() => parseSyncReady({ streamNonce: "", sequence: 0 })).toThrow();
  });

  it("keeps mode, lease, fence, and transport generations exact", () => {
    const session = {
      appId: "app_test",
      streamNonce: "stream_test",
      rtcSessionId: "rtc_test",
      callId: "call_test",
      callEpoch: 3,
      ownerEpoch: 8,
      deviceId: "device_test",
      mode: "monitor",
      leaseId: "lease_monitor",
      fence: 0,
      sdpRevision: 2,
      transportGeneration: 4,
      expiresAt: "2026-07-16T00:01:00Z",
    };
    expect(parseNativeMediaSession(session)).toEqual(session);
    expect(() => parseNativeMediaSession({ ...session, fence: 1 })).toThrow("Invalid native media fence");
    expect(() => parseNativeMediaSession({ ...session, mode: "talk", fence: 0 })).toThrow("Invalid native media fence");
    expect(() => parseNativeMediaSession({ ...session, transportGeneration: 0 })).toThrow();
  });

  it("accepts only bounded offer and ICE events from native WebRTC", () => {
    const session = {
      appId: "app_test",
      streamNonce: "stream_test",
      rtcSessionId: "rtc_test",
      callId: "call_test",
      callEpoch: 3,
      ownerEpoch: 8,
      deviceId: "device_test",
      mode: "talk",
      leaseId: "lease_talk",
      fence: 2,
      sdpRevision: 1,
      transportGeneration: 1,
      expiresAt: "2026-07-16T00:01:00Z",
    } as const;
    expect(parseNativeMediaSignalEvent({
      session,
      signal: { kind: "offer", description: { type: "offer", sdp: "v=0\r\n" } },
    }).signal.kind).toBe("offer");
    expect(parseNativeMediaSignalEvent({
      session,
      signal: { kind: "ice", candidate: { sdpMid: "0", sdpMlineIndex: 0, candidate: "candidate:1" } },
    }).signal.kind).toBe("ice");
    expect(() => parseNativeMediaSignalEvent({
      session,
      signal: { kind: "offer", description: { type: "answer", sdp: "v=0\r\n" } },
    })).toThrow();
  });

  it("accepts only exact-session native WebRTC levels and explicit resets", () => {
    const session = {
      appId: "app_test",
      streamNonce: "stream_test",
      rtcSessionId: "rtc_test",
      callId: "call_test",
      callEpoch: 3,
      ownerEpoch: 8,
      deviceId: "device_test",
      mode: "talk",
      leaseId: "lease_talk",
      fence: 2,
      sdpRevision: 1,
      transportGeneration: 1,
      expiresAt: "2026-07-16T00:01:00Z",
    } as const;
    const levels = {
      session,
      microphoneLevelPermille: 421,
      remoteLevelPermille: 307,
      measuredAt: "2026-07-16T00:00:10.125Z",
    };
    expect(parseNativeMediaLevelsEvent(levels)).toEqual(levels);
    expect(parseNativeMediaLevelsEvent(null)).toBeNull();
    expect(() => parseNativeMediaLevelsEvent({ ...levels, remoteLevelPermille: 1_001 })).toThrow();
    expect(() => parseNativeMediaLevelsEvent({ ...levels, session: { ...session, mode: "monitor" } })).toThrow();
    expect(() => parseNativeMediaLevelsEvent({ ...levels, inventedLevel: 500 })).toThrow();
  });
});

describe("owner-confirmed Desktop peer trust", () => {
  const challenge = {
    challengeId: "peer_trust_1",
    profileId: "profile_1",
    appId: "app_test",
    deviceId: "device_test",
    peerFingerprint: "desktop_key_new",
    rotation: false,
    expiresAt: PEER_TRUST_RECEIVED_AT + 300,
  };

  it("accepts a bounded first-use challenge without any secret material", () => {
    expect(parseDesktopPeerTrustChallenge(challenge, PEER_TRUST_RECEIVED_AT)).toEqual(challenge);
    expect(() => parseDesktopPeerTrustChallenge({ ...challenge, bearer: "must-not-cross-ipc" }, PEER_TRUST_RECEIVED_AT)).toThrow();
  });

  it("requires an exact old/new pair for rotation and rejects stale prompts", () => {
    expect(parseDesktopPeerTrustChallenge({
      ...challenge,
      rotation: true,
      previousPeerFingerprint: "desktop_key_old",
    }, PEER_TRUST_RECEIVED_AT)).toMatchObject({ rotation: true, previousPeerFingerprint: "desktop_key_old" });
    expect(() => parseDesktopPeerTrustChallenge({ ...challenge, rotation: true }, PEER_TRUST_RECEIVED_AT)).toThrow();
    expect(() => parseDesktopPeerTrustChallenge({ ...challenge, expiresAt: PEER_TRUST_RECEIVED_AT }, PEER_TRUST_RECEIVED_AT)).toThrow();
  });
});

describe("owner-confirmed Desktop pairing", () => {
  const desktopThumbprint = "A".repeat(43);
  const mobileThumbprint = "B".repeat(43);
  const desktopFingerprint = Array(16).fill("0123").join(":");
  const mobileFingerprint = Array(16).fill("ABCD").join(":");
  const review = {
    reviewId: "pairing_review_1",
    profileId: "profile_1",
    appId: "app_test",
    workspaceId: "workspace_test",
    desktopConnectionId: "desktop_connection_1",
    deviceId: "mobile_profile_device_1",
    desktopKeyThumbprint: desktopThumbprint,
    desktopFingerprint,
    mobileKeyThumbprint: mobileThumbprint,
    mobileFingerprint,
    issuedAt: PEER_TRUST_RECEIVED_AT,
    expiresAt: PEER_TRUST_RECEIVED_AT + 300,
    unsignedPublicOffer: true,
  } as const;

  it("keeps the unsigned offer label and all profile/key bindings exact", () => {
    expect(parseDesktopPairingReview(review, PEER_TRUST_RECEIVED_AT)).toEqual(review);
    expect(() => parseDesktopPairingReview({ ...review, accessToken: "must-not-cross-ipc" }, PEER_TRUST_RECEIVED_AT)).toThrow();
    expect(() => parseDesktopPairingReview({ ...review, profileId: "profile_2", deviceId: "" }, PEER_TRUST_RECEIVED_AT)).toThrow();
    expect(() => parseDesktopPairingReview({ ...review, unsignedPublicOffer: false }, PEER_TRUST_RECEIVED_AT)).toThrow();
  });

  it("accepts only a response whose public envelope matches the native decision", () => {
    const expiresAt = PEER_TRUST_RECEIVED_AT + 120;
    const responseJson = JSON.stringify({
      kind: "aokie_mobile_pairing_response",
      schemaVersion: 2,
      claims: {
        appId: "app_test",
        workspaceId: "workspace_test",
        desktopConnectionId: "desktop_connection_1",
        desktopKeyThumbprint: desktopThumbprint,
        deviceId: "mobile_profile_device_1",
        displayName: "Reception desk",
        mobileEndpointKey: { algorithm: "ed25519", publicKey: "C".repeat(43), thumbprint: mobileThumbprint },
        pairingNonce: "pairing_nonce_1",
        jti: "pairing_jti_1",
        issuedAt: PEER_TRUST_RECEIVED_AT + 1,
        expiresAt,
      },
      signature: "D".repeat(86),
    });
    const decision = {
      approved: true,
      responseJson,
      desktopKeyThumbprint: desktopThumbprint,
      mobileKeyThumbprint: mobileThumbprint,
      mobileFingerprint,
      deviceId: "mobile_profile_device_1",
      expiresAt,
    };
    expect(parseDesktopPairingDecision(decision)).toEqual(decision);
    expect(() => parseDesktopPairingDecision({ ...decision, deviceId: "mobile_other" })).toThrow("does not match");
    expect(() => parseDesktopPairingDecision({ ...decision, privateKey: "must-not-cross-ipc" })).toThrow();
  });

  it("maps managed admission policy without treating it as OAuth failure", () => {
    expect(parseManagedAdmissionState({ value: "pairing_required", code: "mobile_not_paired", message: "Pair this endpoint" }))
      .toEqual({ value: "pairing_required", code: "mobile_not_paired", message: "Pair this endpoint" });
    expect(() => parseManagedAdmissionState({ value: "policy_denied", code: "mobile_not_paired", message: "wrong mapping" })).toThrow();
    expect(() => parseManagedAdmissionState({ value: "pairing_required", code: "mobile_not_paired", message: "ok", token: "secret" })).toThrow();
  });
});

describe("native custom-server profiles", () => {
  const authorization = {
    authorizationId: "authorization_1",
    profileId: "profile_1",
    origin: "https://formlogic.example/",
    discoveryFingerprint: "server_key_1",
    endpointFingerprint: "mobile_key_1",
    trustState: "first_use",
    expiresAt: PEER_TRUST_RECEIVED_AT + 300,
  };

  it("keeps first-use and rotation fingerprints explicit and bounded", () => {
    expect(parseCustomServerAuthorization(authorization, PEER_TRUST_RECEIVED_AT)).toEqual(authorization);
    expect(parseCustomServerAuthorization({
      ...authorization,
      trustState: "rotation_required",
      discoveryFingerprint: "server_key_2",
      previousDiscoveryFingerprint: "server_key_1",
    }, PEER_TRUST_RECEIVED_AT)).toMatchObject({ trustState: "rotation_required" });
    expect(() => parseCustomServerAuthorization({ ...authorization, accessToken: "must-not-cross-ipc" }, PEER_TRUST_RECEIVED_AT)).toThrow();
    expect(() => parseCustomServerAuthorization({ ...authorization, trustState: "rotation_required" }, PEER_TRUST_RECEIVED_AT)).toThrow();
  });

  it("accepts only one active, trusted native profile and no credentials", () => {
    const profile = {
      profileId: "profile_1",
      serverUrl: "https://formlogic.example/.well-known/aokie-companion",
      origin: "https://formlogic.example/",
      deploymentId: "deployment_1",
      appId: "app_test",
      deviceId: "mobile_1",
      discoveryFingerprint: "server_key_1",
      endpointFingerprint: "mobile_key_1",
      trustState: "trusted",
      active: true,
    };
    expect(parseServerProfiles([profile])).toEqual([profile]);
    expect(() => parseServerProfiles([profile, { ...profile, profileId: "profile_2" }])).toThrow("active");
    expect(() => parseServerProfiles([{ ...profile, refreshToken: "must-not-cross-ipc" }])).toThrow();
  });
});

describe("managed native connection authority", () => {
  const managed = {
    gatewayUrl: "wss://formlogic.example/v2/realtime",
    appId: "app_test",
    deviceId: "device_test",
    accessToken: "",
    protocolVersion: 2,
    managedDeploymentId: "deployment_test",
    managedProfileId: "profile_issuer_a_deployment_test_app_test",
    iceServers: [{ urls: ["stun:stun.example:3478"] }],
    relayOnly: false,
    localPilot: false,
  };

  it("accepts a native-only managed admission reference without a WebView token", () => {
    expect(parseManagedConnectConfig(managed)).toEqual(managed);
    expect(managed.managedProfileId).not.toBe(managed.managedDeploymentId);
    expect(managed.managedProfileId).not.toBe(managed.appId);
  });

  it("rejects renderer-visible tokens and authority-shape changes", () => {
    expect(() => parseManagedConnectConfig({ ...managed, accessToken: "must-not-enter-webview" }))
      .toThrow("Invalid managed connection authority");
    expect(() => parseManagedConnectConfig({ ...managed, protocolVersion: 1 }))
      .toThrow("Invalid managed connection authority");
    const missingProfile: Record<string, unknown> = { ...managed };
    delete missingProfile.managedProfileId;
    expect(() => parseManagedConnectConfig(missingProfile)).toThrow("managed profile ID");
    const missingDeployment: Record<string, unknown> = { ...managed };
    delete missingDeployment.managedDeploymentId;
    expect(() => parseManagedConnectConfig(missingDeployment)).toThrow("managed deployment ID");
    expect(() => parseManagedConnectConfig({ ...managed, managedProfileId: "profile with spaces" }))
      .toThrow("managed profile ID");
    expect(() => parseManagedConnectConfig({ ...managed, extraAuthority: true })).toThrow();
  });

  it("keeps the local-pilot authority explicit and never permits LAN plain WebSocket", () => {
    const local = {
      ...managed,
      gatewayUrl: "ws://127.0.0.1:8787/v2/realtime",
      localPilot: true,
    };
    expect(parseManagedConnectConfig(local)).toEqual(local);
    expect(() => parseManagedConnectConfig({
      ...local,
      gatewayUrl: "ws://192.168.1.10:8787/v2/realtime",
    })).toThrow("Invalid managed gateway URL");
  });
});

describe("managed Companion account data", () => {
  const bootstrap = {
    membership: { appId: "app_test", appSlug: "my-business", status: "active" },
    device: {
      id: "device_record_1",
      userId: "user_1",
      appId: "app_test",
      subjectId: "installation_1",
      role: "mobile",
      displayName: "Reception desk",
      grants: ["state_read", "end_caller"],
      approvedAt: "2026-07-16T03:00:00Z",
      lastSeenAt: "2026-07-16T03:01:00Z",
      revokedAt: null,
    },
    capabilities: ["state_read", "end_caller"],
    availability: null,
    routingGroups: [],
    staff: [{
      id: "staff_1", displayName: "Test User", roleName: "Owner",
      isCurrentUser: true, isOwner: true, companionReady: true,
    }],
    history: { activity: [], sessions: [] },
    pushEndpoints: [],
  };

  it("accepts only exact redacted bootstrap data", () => {
    expect(parseCompanionBootstrap(bootstrap)).toMatchObject({
      membership: { appId: "app_test" },
      capabilities: ["state_read", "end_caller"],
    });
    expect(() => parseCompanionBootstrap({ ...bootstrap, accessToken: "must-never-cross-ipc" })).toThrow();
    const { revokedAt: _removed, ...missingRevocation } = bootstrap.device;
    expect(() => parseCompanionBootstrap({ ...bootstrap, device: missingRevocation })).toThrow();
  });

  it("accepts the complete managed participant capability set", () => {
    const capabilities = [
      "state_read", "caller_read", "captions_read", "participants_read",
      "participant_identity_read", "audio_levels_read", "monitor", "consult",
      "takeover", "resume_aokie", "rtc_signal", "assistance_read",
      "assistance_respond", "end_caller",
    ];
    const parsed = parseCompanionBootstrap({
      ...bootstrap,
      device: { ...bootstrap.device, grants: capabilities },
      capabilities,
    });

    expect(parsed.capabilities).toEqual(capabilities);
    expect(parsed.device.grants).toEqual(capabilities);
  });

  it("requires one current staff identity and cross-checks known routing staff", () => {
    expect(() => parseCompanionBootstrap({
      ...bootstrap,
      staff: [{ ...bootstrap.staff[0], isCurrentUser: false }],
    })).toThrow("current staff identity");
    expect(parseCompanionBootstrap({ ...bootstrap, staff: [] }).staff).toEqual([]);

    const routingMember = {
      staffId: "staff_1", displayName: "Test User", roleName: "Owner", isCurrentUser: true,
      isCurrentDevice: true,
      priority: 1, enabled: true, availability: "available",
      availabilityUpdatedAt: "2026-07-16T03:00:00Z", availabilityExpiresAt: null,
    };
    const withRouting = {
      ...bootstrap,
      routingGroups: [{ id: "group_1", name: "Primary", policy: "priority", enabled: true, members: [routingMember] }],
    };
    expect(parseCompanionBootstrap(withRouting).routingGroups[0].members[0].staffId).toBe("staff_1");
    expect(() => parseCompanionBootstrap({
      ...withRouting,
      routingGroups: [{ ...withRouting.routingGroups[0], members: [{ ...routingMember, displayName: "Wrong User" }] }],
    })).toThrow("routing staff identity");
    expect(() => parseCompanionBootstrap({
      ...withRouting,
      routingGroups: [{ ...withRouting.routingGroups[0], members: [{ ...routingMember, isCurrentUser: false }] }],
    })).toThrow("Invalid Companion routing");
    expect(parseCompanionBootstrap({
      ...withRouting,
      routingGroups: [{ ...withRouting.routingGroups[0], members: [{ ...routingMember, staffId: "staff_outside_cap" }] }],
    }).routingGroups[0].members[0].staffId).toBe("staff_outside_cap");
  });

  it("requires strict nullable availability and canonicalizes legacy database UTC", () => {
    const available = {
      appId: "app_test",
      deviceId: "installation_1",
      availability: {
        availability: "available",
        updatedAt: "2026-07-16T03:00:00Z",
        expiresAt: null,
      },
    };
    expect(parseCompanionAvailability(available)).toEqual(available);
    expect(parseCompanionAvailability({
      ...available,
      availability: { ...available.availability, updatedAt: "2026-07-16 03:00:00" },
    }).availability?.updatedAt).toBe("2026-07-16T03:00:00Z");
    const { expiresAt: _expiry, ...missingExpiry } = available.availability;
    expect(() => parseCompanionAvailability({ ...available, availability: missingExpiry })).toThrow();
  });

  it("rejects unknown history event classes", () => {
    expect(() => parseCompanionHistory({
      activity: [{
        id: "row_1", eventId: "event_1", appId: "app_test", sessionRecordId: null,
        callId: null, deviceId: null, actorUserId: null, subjectId: "installation_1",
        eventType: "caller_transcript", mode: null, reason: null, ownerEpoch: null,
        occurredAt: "2026-07-16 03:00:00",
      }],
      sessions: [],
    })).toThrow();
  });

  it("accepts bounded redacted FormLogic call records and rejects full phone numbers", () => {
    const record = {
      id: "record_1", callId: "call_1", callerName: "Customer", maskedNumber: "••• 782",
      status: "completed", direction: "inbound", summary: "Booking captured.",
      startedAt: "2026-07-16T03:00:00Z", endedAt: "2026-07-16T03:04:00Z",
      durationSeconds: 240, followUpRequired: false, submittedAt: "2026-07-16T03:00:00Z",
    };
    expect(parseCompanionCallRecords({ records: [record], access: "full" }).records[0]).toEqual(record);
    expect(() => parseCompanionCallRecords({ records: [{ ...record, maskedNumber: "+61 491 570 156" }], access: "full" })).toThrow("redacted");
    expect(() => parseCompanionCallRecords({ records: [record], access: "full", accessToken: "secret" })).toThrow();
    expect(() => parseCompanionCallRecords({ records: [record], access: "none" })).toThrow("access denied");
  });

  it("parses exact call detail without accepting hidden payload fields", () => {
    const record = {
      id: "record_1", callId: "call_1", callerName: null, maskedNumber: null,
      status: "missed", direction: "unknown", summary: null, startedAt: null, endedAt: null,
      durationSeconds: null, followUpRequired: true, submittedAt: "2026-07-16T03:00:00Z",
    };
    const detail = {
      record,
      transcript: [{ id: "turn_1", speaker: "caller", text: "Please call back.", occurredAt: "2026-07-16T03:00:01Z" }],
      followUps: [{ id: "task_1", summary: "Return the call", status: "open", priority: "high", submittedAt: "2026-07-16T03:01:00Z" }],
    };
    expect(parseCompanionCallRecordDetail(detail)).toEqual(detail);
    expect(parseCompanionCallRecordDetail(detail, "record_1")).toEqual(detail);
    expect(() => parseCompanionCallRecordDetail(detail, "record_other")).toThrow("requested record");
    expect(() => parseCompanionCallRecordDetail({
      ...detail,
      transcript: [...detail.transcript, { ...detail.transcript[0] }],
    })).toThrow("transcript turns");
    expect(() => parseCompanionCallRecordDetail({
      ...detail,
      followUps: [...detail.followUps, { ...detail.followUps[0] }],
    })).toThrow("follow-ups");
    expect(() => parseCompanionCallRecordDetail({ ...detail, rawAudio: "forbidden" })).toThrow();
  });
});

describe("protocol-v2 authoritative idle sync", () => {
  const idle = {
    kind: "idle_sync",
    schemaVersion: 2,
    appId: "app_test",
    sequence: 7,
    grants: ["state_read", "caller_read"],
  };

  it("accepts only an app-bound, sequenced no-call proof", () => {
    expect(parseV2IdleSync(idle)).toEqual(idle);
    expect(() => parseV2IdleSync({ ...idle, sequence: 0 })).toThrow("sequence");
    expect(() => parseV2IdleSync({ ...idle, extra: true })).toThrow();
  });

  it("requires a unique state_read grant set", () => {
    expect(() => parseV2IdleSync({ ...idle, grants: ["caller_read"] })).toThrow("grants");
    expect(() => parseV2IdleSync({ ...idle, grants: ["state_read", "state_read"] })).toThrow("grants");
    expect(() => parseV2IdleSync({ ...idle, grants: ["state_read", "dongle_control"] })).toThrow("grant");
  });
});

describe("protocol-v2 assistance acknowledgement", () => {
  it("accepts only a positive exact server acknowledgement", () => {
    const accepted = {
      kind: "assistance_answer_accepted", schemaVersion: 2, appId: "app_test",
      requestId: "request_1", answerId: "answer_1", accepted: true,
    };
    expect(parseV2AssistanceAnswerAccepted(accepted)).toEqual(accepted);
    expect(() => parseV2AssistanceAnswerAccepted({ ...accepted, accepted: false })).toThrow();
    expect(() => parseV2AssistanceAnswerAccepted({ ...accepted, answer: "private text" })).toThrow();
  });
});

describe("protocol-v2 projected call truth", () => {
  const snapshot = {
    kind: "snapshot",
    schemaVersion: 2,
    appId: "app_test",
    sequence: 9,
    grants: ["state_read", "rtc_signal", "takeover", "resume_aokie"],
    snapshot: {
      callId: "call_test",
      callEpoch: 2,
      ownerEpoch: 3,
      switchboardRevision: 4,
      remoteRevision: 5,
      telephonyState: "active",
      serviceMode: "aokie_active",
      mediaState: "ready",
      remoteCapabilities: {
        softwareHold: true,
        carrierHoldEvidence: "proven",
        secondaryCallObservation: "observed",
        voiceConsult: true,
        takeover: true,
      },
      secondaryCallPolicy: "miss_and_callback",
      secondaryCall: { stable: true, callbackEligible: true, status: "queued", waitingCallId: "waiting_1" },
      remoteConsent: {
        policyId: "policy_test",
        policyVersion: 1,
        enabled: true,
        acknowledged: true,
        acknowledgedAt: "2026-07-16T00:00:00Z",
        captionsEnabled: true,
        assistanceEnabled: true,
        monitorEnabled: true,
        consultEnabled: true,
        takeoverEnabled: true,
      },
      caller: { label: "Caller", maskedNumber: "••• 431" },
      captions: [],
      participants: [{ participantId: "participant_1", mode: "observer", state: "connected", displayLabel: "Jordan" }],
      audioLevels: [
        { source: "caller", levelPermille: 320 },
        { source: "aokie", levelPermille: 180 },
        { source: "companion", participantId: "participant_1", levelPermille: 75 },
      ],
      pendingMobileOffers: [{
        offer: {
          offerId: "offer_1",
          opportunityId: "opportunity_1",
          targetDeviceId: "device_test",
          targetHolderKeyThumbprint: "thumbprint_test",
          offeredMode: "takeover",
          surface: "in_app",
          appId: "app_test",
          callId: "call_test",
          callEpoch: 2,
          ownerEpoch: 3,
          switchboardRevision: 4,
          remoteRevision: 5,
          requiredConsentPolicyId: "policy_test",
          requiredConsentPolicyVersion: 1,
          requiredGrants: ["state_read", "rtc_signal", "takeover"],
          issuedAt: 2_000_000_000,
          expiresAt: 2_000_000_030,
          jti: "offer_jti_1",
        },
        offerToken: "signed.offer.token",
      }],
      occurredAt: "2026-07-16T00:00:00Z",
    },
  };

  it("accepts endpoint-reported levels, participants, secondary-call truth, and exact offers", () => {
    expect(parseV2Snapshot(snapshot)).toMatchObject({
      snapshot: { participants: [{ displayLabel: "Jordan" }], pendingMobileOffers: [{ offer: { offeredMode: "takeover" } }] },
    });
  });

  it("rejects invented level identities and cross-call offers", () => {
    expect(() => parseV2Snapshot({
      ...snapshot,
      snapshot: { ...snapshot.snapshot, audioLevels: [{ source: "companion", participantId: "unknown_peer", levelPermille: 10 }] },
    })).toThrow("audio-level source binding");
    expect(() => parseV2Snapshot({
      ...snapshot,
      snapshot: {
        ...snapshot.snapshot,
        pendingMobileOffers: [{
          ...snapshot.snapshot.pendingMobileOffers[0],
          offer: { ...snapshot.snapshot.pendingMobileOffers[0].offer, callId: "other_call" },
        }],
      },
    })).toThrow("Stale or mismatched");
  });

  it("rejects offers from an older revision but accepts an offer-free reconciliation", () => {
    const advanced = {
      ...snapshot,
      sequence: snapshot.sequence + 1,
      snapshot: {
        ...snapshot.snapshot,
        remoteRevision: snapshot.snapshot.remoteRevision + 1,
        serviceMode: "human_active",
        mediaState: "active",
        companionMicrophoneMuted: true,
      },
    };
    expect(() => parseV2Snapshot(advanced)).toThrow("Stale or mismatched");
    expect(parseV2Snapshot({
      ...advanced,
      snapshot: { ...advanced.snapshot, pendingMobileOffers: [] },
    })).toMatchObject({
      snapshot: {
        remoteRevision: 6,
        companionMicrophoneMuted: true,
        pendingMobileOffers: [],
      },
    });
  });
});

describe("protocol-v2 caller-ending events", () => {
  const authority = {
    appId: "app_test",
    confirmationId: "confirmation_test",
    deviceId: "device_test",
    callId: "call_test",
    callEpoch: 2,
    ownerEpoch: 3,
    switchboardRevision: 4,
    remoteRevision: 5,
    leaseId: "lease_test",
    fence: 6,
  };

  it("accepts a short-lived, redacted challenge", () => {
    const challenge = {
      kind: "end_caller_challenge",
      schemaVersion: 2,
      requestId: "request_test",
      expiresAt: Math.floor(Date.now() / 1_000) + 30,
      ...authority,
    };
    expect(parseV2EndCaller(challenge)).toEqual(challenge);
    expect(() => parseV2EndCaller({ ...challenge, nonce: "must-not-enter-webview" })).toThrow();
  });

  it("requires an accepted submission and fully bound result", () => {
    expect(parseV2EndCaller({
      kind: "end_caller_submitted",
      schemaVersion: 2,
      appId: authority.appId,
      requestId: "request_test",
      operationId: "operation_test",
      confirmationId: authority.confirmationId,
      accepted: true,
    })).toMatchObject({ accepted: true });
    expect(parseV2EndCaller({
      kind: "end_caller_result",
      schemaVersion: 2,
      operationId: "operation_test",
      outcome: "completed",
      ...authority,
    })).toMatchObject({ outcome: "completed", fence: 6 });
    expect(() => parseV2EndCaller({
      kind: "end_caller_result",
      schemaVersion: 2,
      operationId: "operation_test",
      outcome: "completed",
      ...authority,
      fence: 0,
    })).toThrow();
  });

  it("accepts only a typed, bounded pre-relay failure", () => {
    expect(parseV2EndCaller({
      kind: "end_caller_failure",
      schemaVersion: 2,
      requestId: "request_test",
      code: "stale_authority",
      message: "The active owner changed.",
    })).toMatchObject({ code: "stale_authority" });
    expect(() => parseV2EndCaller({
      kind: "end_caller_failure",
      schemaVersion: 2,
      requestId: "request_test",
      code: "stale_authority",
      message: "The active owner changed.",
      operationId: "not_available_before_relay",
    })).toThrow();
  });
});

describe("native audio endpoint state", () => {
  it("accepts a strict selectable desktop catalog", () => {
    const devices = {
      routingPolicy: "selectable",
      inputDevices: [
        { id: "system_default", label: "System default" },
        { id: "{capture-guid}", label: "USB microphone" },
      ],
      outputDevices: [
        { id: "system_default", label: "System default" },
        { id: "{playout-guid}", label: "USB speakers" },
      ],
      selectedInputId: "{capture-guid}",
      selectedOutputId: "{playout-guid}",
      state: "idle",
      canSelect: true,
    };
    expect(parseNativeAudioDevices(devices)).toEqual(devices);
    expect(() => parseNativeAudioDevices({ ...devices, state: "media_active", canSelect: true })).toThrow();
    expect(() => parseNativeAudioDevices({
      ...devices,
      inputDevices: [...devices.inputDevices, devices.inputDevices[1]],
    })).toThrow();
  });

  it("accepts Android system-owned routes while keeping idle selection locked", () => {
    const managed = {
      routingPolicy: "system_managed",
      inputDevices: [{ id: "speaker:7", label: "Speaker" }, { id: "wired:9", label: "Wired or USB headset" }],
      outputDevices: [{ id: "speaker:7", label: "Speaker" }, { id: "wired:9", label: "Wired or USB headset" }],
      selectedInputId: "system_managed",
      selectedOutputId: "system_managed",
      state: "idle",
      canSelect: false,
    };
    expect(parseNativeAudioDevices(managed)).toEqual(managed);
    expect(() => parseNativeAudioDevices({ ...managed, canSelect: true })).toThrow();
    expect(() => parseNativeAudioDevices({ ...managed, outputDevices: managed.outputDevices.slice(1) })).toThrow();
  });

  it("accepts Android in-call route selection only for a matched current route", () => {
    const active = {
      routingPolicy: "system_managed",
      inputDevices: [{ id: "speaker:7", label: "Speaker" }, { id: "bluetooth:11", label: "Bluetooth or hearing device" }],
      outputDevices: [{ id: "speaker:7", label: "Speaker" }, { id: "bluetooth:11", label: "Bluetooth or hearing device" }],
      selectedInputId: "bluetooth:11",
      selectedOutputId: "bluetooth:11",
      state: "microphone_active",
      canSelect: true,
    };
    expect(parseNativeAudioDevices(active)).toEqual(active);
    expect(() => parseNativeAudioDevices({ ...active, selectedInputId: "speaker:7" })).toThrow();
    expect(() => parseNativeAudioDevices({ ...active, selectedInputId: "bluetooth:99", selectedOutputId: "bluetooth:99" })).toThrow();
    expect(() => parseNativeAudioDevices({ ...active, canSelect: false })).toThrow();
  });
});
