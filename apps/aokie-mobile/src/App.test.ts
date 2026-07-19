import { describe, expect, it } from "vitest";
import {
  assistanceMatchesV2Call,
  assistanceExpiryDelayMs,
  currentV2InAppOffer,
  currentV2AutoArmAttempt,
  currentV2AutoArmSnapshot,
  desktopPairingOpenAfterAdmission,
  formatAssistanceCountdown,
  INITIAL_V2_TAKEOVER_ATTEMPT,
  isExpectedPreparedTakeoverReplacement,
  isRetryableMediaArmFailure,
  shouldAutoArmConfirmedTakeover,
  shouldAcceptV2AuthoritativeSequence,
  shouldAcceptV2MicrophoneMuteReconciliation,
  takeoverConfirmationMode,
  v2CurrentAccessPolicyPresentation,
  v2AssistanceGrantAccess,
  v2LeaseExitLabel,
  v2LeaseRevokeAllowed,
  v2ParticipantAccess,
  v2RecoveryProgressStages,
  v2RemoteAudioProofLabel,
  v2RemoteConsentCurrent,
  v2RouteProgressStages,
  v2RuntimeFailureReducer,
  v2SnapshotAuthorizesConfirmedTakeoverArm,
  v2SnapshotMaintainsConfirmedTakeoverAuthority,
  v2TakeoverAttemptReducer,
  v2TransferTranscript,
  type ConfirmedTakeoverTarget,
} from "./App";
import type { NativeMediaSession, NativeMediaStateEvent, V2AssistanceRequestEvent, V2CallSnapshotEvent, V2LeaseEvent } from "./bridge";

const OFFER_NOW = 1_800_000_000;

describe("formatAssistanceCountdown", () => {
  it("formats a bounded minute and second countdown", () => {
    expect(formatAssistanceCountdown(65.9)).toBe("1:05");
    expect(formatAssistanceCountdown(9)).toBe("0:09");
    expect(formatAssistanceCountdown(-1)).toBe("0:00");
    expect(formatAssistanceCountdown(Number.NaN)).toBe("0:00");
  });
});

describe("relay assistance controls", () => {
  it("keeps a readable help request eligible for voice Consult while text reply is withheld", () => {
    expect(v2AssistanceGrantAccess(["state_read", "assistance_read", "consult"]))
      .toEqual({ readable: true, answerable: false });
    expect(v2AssistanceGrantAccess([
      "state_read", "assistance_read", "assistance_respond", "consult",
    ])).toEqual({ readable: true, answerable: true });
  });
});

describe("native remote-audio proof labels", () => {
  it("does not describe the private Aokie Consult track as caller audio", () => {
    expect(v2RemoteAudioProofLabel("consult", true)).toBe("Private Aokie audio ready");
    expect(v2RemoteAudioProofLabel("consult", false)).toBe("Waiting for private Aokie audio");
    expect(v2RemoteAudioProofLabel("prepared_consult", false)).toBe("Waiting for private Aokie audio");
    expect(v2RemoteAudioProofLabel("talk", true)).toBe("Caller audio ready");
    expect(v2RemoteAudioProofLabel("monitor", false)).toBe("Waiting for caller audio");
  });
});

type OfferClaims = V2CallSnapshotEvent["snapshot"]["pendingMobileOffers"][number]["offer"];

function takeoverSnapshot(offerOverrides: Partial<OfferClaims> = {}): V2CallSnapshotEvent {
  const offer: OfferClaims = {
    offerId: "offer_takeover_a",
    opportunityId: "opportunity_takeover_a",
    targetDeviceId: "device_a",
    targetHolderKeyThumbprint: "holder_key_a",
    offeredMode: "takeover",
    surface: "in_app",
    appId: "app_a",
    callId: "call_a",
    callEpoch: 7,
    ownerEpoch: 4,
    switchboardRevision: 11,
    remoteRevision: 13,
    requiredConsentPolicyId: "aokie_remote_access",
    requiredConsentPolicyVersion: 3,
    requiredGrants: ["state_read", "rtc_signal", "takeover"],
    issuedAt: OFFER_NOW - 1,
    expiresAt: OFFER_NOW + 20,
    jti: "offer_jti_takeover_a",
    ...offerOverrides,
  };
  return {
    kind: "snapshot",
    schemaVersion: 2,
    appId: "app_a",
    sequence: 8,
    grants: ["state_read", "rtc_signal", "monitor", "consult", "takeover", "resume_aokie"],
    snapshot: {
      callId: "call_a",
      callEpoch: 7,
      ownerEpoch: 4,
      switchboardRevision: 11,
      remoteRevision: 13,
      telephonyState: "active",
      serviceMode: "aokie_active",
      mediaState: "ready",
      remoteCapabilities: {
        softwareHold: true,
        carrierHoldEvidence: "unknown",
        secondaryCallObservation: "unknown",
        voiceConsult: true,
        takeover: true,
      },
      secondaryCallPolicy: "normal",
      remoteConsent: {
        policyId: "aokie_remote_access",
        policyVersion: 3,
        enabled: true,
        acknowledged: true,
        acknowledgedAt: "2027-01-15T08:00:00Z",
        captionsEnabled: false,
        assistanceEnabled: true,
        monitorEnabled: true,
        consultEnabled: true,
        takeoverEnabled: true,
      },
      participants: [],
      companionMicrophoneMuted: false,
      pendingMobileOffers: [{ offer, offerToken: "signed.offer.token" }],
      occurredAt: "2027-01-15T08:00:00Z",
    },
  };
}

function confirmedTakeoverTarget(): ConfirmedTakeoverTarget {
  const offer = takeoverSnapshot().snapshot.pendingMobileOffers[0].offer;
  return {
    appId: offer.appId,
    callId: offer.callId,
    callEpoch: offer.callEpoch,
    ownerEpoch: offer.ownerEpoch,
    switchboardRevision: offer.switchboardRevision,
    remoteRevision: offer.remoteRevision,
    targetDeviceId: offer.targetDeviceId,
    requiredConsentPolicyId: offer.requiredConsentPolicyId,
    requiredConsentPolicyVersion: offer.requiredConsentPolicyVersion,
    requiredGrants: [...offer.requiredGrants],
  };
}

function activeTakeoverSnapshot(): V2CallSnapshotEvent {
  const snapshot = takeoverSnapshot();
  snapshot.sequence += 1;
  snapshot.snapshot.ownerEpoch += 1;
  snapshot.snapshot.serviceMode = "human_active";
  snapshot.snapshot.mediaState = "active";
  snapshot.snapshot.pendingMobileOffers = [];
  return snapshot;
}

function takeoverMedia(ownerEpoch = 5): { lease: V2LeaseEvent; media: NativeMediaStateEvent } {
  const session: NativeMediaSession = {
    appId: "app_a",
    streamNonce: "stream_a",
    rtcSessionId: "rtc_a",
    callId: "call_a",
    callEpoch: 7,
    ownerEpoch,
    deviceId: "device_a",
    mode: "talk",
    leaseId: "lease_a",
    fence: 1,
    sdpRevision: 1,
    transportGeneration: 1,
    expiresAt: "2099-01-01T00:00:00Z",
  };
  return {
    lease: { session, mode: "takeover", phase: "active", provisional: false },
    media: { session, phase: "remote_audio_ready", microphoneActive: false, remoteAudioReady: true },
  };
}

describe("desktopPairingOpenAfterAdmission", () => {
  it("opens automatically when native admission requires pairing", () => {
    expect(desktopPairingOpenAfterAdmission(false, "pairing_required")).toBe(true);
  });

  it("does not close a pairing sheet the owner opened manually", () => {
    expect(desktopPairingOpenAfterAdmission(true, "desktop_unavailable")).toBe(true);
  });

  it("does not open the sheet for unrelated admission states", () => {
    expect(desktopPairingOpenAfterAdmission(false, "desktop_unavailable")).toBe(false);
  });
});

describe("shouldAcceptV2AuthoritativeSequence", () => {
  it("keeps one high-water mark across snapshots and authenticated idle state", () => {
    let lastSequence = 8;
    expect(shouldAcceptV2AuthoritativeSequence(lastSequence, 9)).toBe(true);
    lastSequence = 9;
    expect(shouldAcceptV2AuthoritativeSequence(lastSequence, 8)).toBe(false);
    expect(shouldAcceptV2AuthoritativeSequence(lastSequence, 9)).toBe(false);
    expect(shouldAcceptV2AuthoritativeSequence(lastSequence, 10)).toBe(true);
  });

  it("rejects invalid sequence values", () => {
    expect(shouldAcceptV2AuthoritativeSequence(0, 0)).toBe(false);
    expect(shouldAcceptV2AuthoritativeSequence(0, Number.NaN)).toBe(false);
    expect(shouldAcceptV2AuthoritativeSequence(0, Number.MAX_SAFE_INTEGER + 1)).toBe(false);
  });
});

describe("shouldAcceptV2MicrophoneMuteReconciliation", () => {
  it("accepts only an offer-free same-sequence mute projection on the exact call", () => {
    const current = activeTakeoverSnapshot();
    current.snapshot.pendingMobileOffers = takeoverSnapshot().snapshot.pendingMobileOffers;
    const reconciled = structuredClone(current);
    reconciled.snapshot.remoteRevision += 1;
    reconciled.snapshot.companionMicrophoneMuted = true;
    reconciled.snapshot.pendingMobileOffers = [];

    expect(shouldAcceptV2MicrophoneMuteReconciliation(
      current,
      reconciled,
      current.sequence,
    )).toBe(true);

    const unmuted = structuredClone(reconciled);
    unmuted.snapshot.remoteRevision += 1;
    unmuted.snapshot.companionMicrophoneMuted = false;
    expect(shouldAcceptV2MicrophoneMuteReconciliation(
      reconciled,
      unmuted,
      current.sequence,
    )).toBe(true);

    const retainedOffer = structuredClone(reconciled);
    retainedOffer.snapshot.pendingMobileOffers = current.snapshot.pendingMobileOffers;
    expect(shouldAcceptV2MicrophoneMuteReconciliation(current, retainedOffer, current.sequence)).toBe(false);

    const changedCallFact = structuredClone(reconciled);
    changedCallFact.snapshot.ownerEpoch += 1;
    expect(shouldAcceptV2MicrophoneMuteReconciliation(current, changedCallFact, current.sequence)).toBe(false);

    const inventedSequence = structuredClone(reconciled);
    inventedSequence.sequence += 1;
    expect(shouldAcceptV2MicrophoneMuteReconciliation(current, inventedSequence, current.sequence)).toBe(false);
  });
});

describe("assistanceExpiryDelayMs", () => {
  it("expires locally and caps a hostile far-future timeout", () => {
    expect(assistanceExpiryDelayMs(1_800_000_000, 1_800_000_000_000)).toBe(0);
    expect(assistanceExpiryDelayMs(1_800_000_001, 1_800_000_000_000)).toBe(1_000);
    expect(assistanceExpiryDelayMs(Number.MAX_SAFE_INTEGER, 0)).toBe(2_147_000_000);
    expect(assistanceExpiryDelayMs(Number.NaN, 0)).toBe(0);
  });
});

describe("currentV2InAppOffer", () => {
  it("returns the single current foreground offer bound to this device and call", () => {
    expect(currentV2InAppOffer(takeoverSnapshot(), "device_a", "takeover", OFFER_NOW)?.offer.offerId)
      .toBe("offer_takeover_a");
  });

  it("does not let the foreground control consume a voice-system-UI offer", () => {
    const snapshot = takeoverSnapshot({ surface: "voice_system_ui" });
    expect(currentV2InAppOffer(snapshot, "device_a", "takeover", OFFER_NOW)).toBeNull();
  });

  it("expires locally without waiting for another gateway snapshot", () => {
    const snapshot = takeoverSnapshot({ expiresAt: OFFER_NOW });
    expect(currentV2InAppOffer(snapshot, "device_a", "takeover", OFFER_NOW)).toBeNull();
  });

  it("rejects wrong-device, future, and call-fence mismatches", () => {
    expect(currentV2InAppOffer(takeoverSnapshot(), "device_b", "takeover", OFFER_NOW)).toBeNull();
    expect(currentV2InAppOffer(takeoverSnapshot({ issuedAt: OFFER_NOW + 6 }), "device_a", "takeover", OFFER_NOW)).toBeNull();
    expect(currentV2InAppOffer(takeoverSnapshot({ remoteRevision: 14 }), "device_a", "takeover", OFFER_NOW)).toBeNull();
  });

  it("stays locked when more than one offer could satisfy the same foreground action", () => {
    const snapshot = takeoverSnapshot();
    const duplicate = structuredClone(snapshot.snapshot.pendingMobileOffers[0]);
    duplicate.offer.offerId = "offer_takeover_b";
    duplicate.offer.jti = "offer_jti_takeover_b";
    snapshot.snapshot.pendingMobileOffers.push(duplicate);
    expect(currentV2InAppOffer(snapshot, "device_a", "takeover", OFFER_NOW)).toBeNull();
  });

  it("keeps a transfer offer out of generic takeover and selects only its exact request", () => {
    const snapshot = takeoverSnapshot({
      acceptedTransferRequestId: "assistance_transfer_a",
      requiredGrants: ["state_read", "rtc_signal", "takeover", "assistance_respond"],
    });
    snapshot.grants.push("assistance_respond");
    expect(currentV2InAppOffer(snapshot, "device_a", "takeover", OFFER_NOW)).toBeNull();
    expect(currentV2InAppOffer(
      snapshot,
      "device_a",
      "takeover",
      OFFER_NOW,
      "assistance_transfer_a",
    )?.offer.offerId).toBe("offer_takeover_a");
    expect(currentV2InAppOffer(
      snapshot,
      "device_a",
      "takeover",
      OFFER_NOW,
      "assistance_transfer_b",
    )).toBeNull();
  });
});

describe("v2ParticipantAccess", () => {
  it("fails roster, identity, and levels closed to consent and their own grants", () => {
    expect(v2ParticipantAccess([], true)).toEqual({ roster: false, identity: false, levels: false });
    expect(v2ParticipantAccess(["participants_read"], true)).toEqual({
      roster: true,
      identity: false,
      levels: false,
    });
    expect(v2ParticipantAccess([
      "participants_read",
      "participant_identity_read",
      "audio_levels_read",
    ], true)).toEqual({ roster: true, identity: true, levels: true });
    expect(v2ParticipantAccess([
      "participants_read",
      "participant_identity_read",
      "audio_levels_read",
    ], false)).toEqual({ roster: false, identity: false, levels: false });
  });

  it("withdraws participant and meter access when consent expires or is malformed", () => {
    const consent = takeoverSnapshot().snapshot.remoteConsent;
    const now = Date.parse("2027-01-15T08:00:00Z");
    expect(v2RemoteConsentCurrent(true, { ...consent, expiresAt: "2027-01-15T08:01:00Z" }, now)).toBe(true);
    expect(v2RemoteConsentCurrent(true, { ...consent, expiresAt: "2027-01-15T07:59:59Z" }, now)).toBe(false);
    expect(v2RemoteConsentCurrent(true, { ...consent, expiresAt: "not-a-time" }, now)).toBe(false);
    expect(v2RemoteConsentCurrent(false, consent, now)).toBe(false);
  });
});

describe("v2TransferTranscript", () => {
  it("shows only bounded final captions under an exact transfer, consent, and grant fence", () => {
    const snapshot = takeoverSnapshot();
    snapshot.grants.push("assistance_read", "captions_read");
    snapshot.snapshot.remoteConsent.captionsEnabled = true;
    snapshot.snapshot.captions = [
      { captionId: "caption_1", speaker: "Caller", text: "I need to move my booking.", occurredAt: "2027-01-15T08:00:00Z", finalText: true },
      { captionId: "caption_2", speaker: "Aokie", text: "One moment please.", occurredAt: "2027-01-15T08:00:01Z", finalText: true },
      { captionId: "caption_partial", speaker: "Caller", text: "part", occurredAt: "2027-01-15T08:00:02Z", finalText: false },
    ];
    const request: V2AssistanceRequestEvent = {
      kind: "assistance_request",
      schemaVersion: 2,
      appId: "app_a",
      eventId: "event_transfer_a",
      requestId: "transfer_request_a",
      callId: "call_a",
      callEpoch: 7,
      ownerEpoch: 4,
      switchboardRevision: 11,
      remoteRevision: 13,
      question: "Can you take this caller?",
      transferOffered: true,
      expiresAt: OFFER_NOW + 20,
    };
    expect(v2TransferTranscript(snapshot, request, 1)).toEqual({
      available: true,
      captions: [snapshot.snapshot.captions[1]],
    });
  });

  it("does not expose captions without the exact grant and transfer fence", () => {
    const snapshot = takeoverSnapshot();
    snapshot.snapshot.remoteConsent.captionsEnabled = true;
    snapshot.snapshot.captions = [
      { captionId: "caption_private", speaker: "Caller", text: "Private detail", occurredAt: "2027-01-15T08:00:00Z", finalText: true },
    ];
    const request: V2AssistanceRequestEvent = {
      kind: "assistance_request",
      schemaVersion: 2,
      appId: "app_a",
      eventId: "event_transfer_a",
      requestId: "transfer_request_a",
      callId: "call_a",
      callEpoch: 7,
      ownerEpoch: 4,
      switchboardRevision: 11,
      remoteRevision: 13,
      question: "Can you take this caller?",
      transferOffered: true,
      expiresAt: OFFER_NOW + 20,
    };
    expect(v2TransferTranscript(snapshot, request).captions).toEqual([]);
    snapshot.grants.push("assistance_read", "captions_read");
    request.remoteRevision = 14;
    expect(v2TransferTranscript(snapshot, request).captions).toEqual([]);
  });
});

describe("v2RuntimeFailureReducer", () => {
  const failure = { message: "signed offer rejected", operation: "takeover" as const, occurredAt: 123 };

  it("keeps a V2 failure visible when authoritative idle follows it", () => {
    const reported = v2RuntimeFailureReducer(null, { type: "report", failure });
    expect(v2RuntimeFailureReducer(reported, { type: "authoritative_idle" })).toEqual(failure);
  });

  it("clears a recovered realtime failure after authoritative idle sync", () => {
    const realtimeFailure = { message: "transport closed before authoritative sync", operation: "realtime" as const, occurredAt: 124 };
    const reported = v2RuntimeFailureReducer(null, { type: "report", failure: realtimeFailure });
    expect(v2RuntimeFailureReducer(reported, { type: "authoritative_idle" })).toBeNull();
  });

  it("clears stale failure only for dismissal or a newly confirmed takeover", () => {
    const reported = v2RuntimeFailureReducer(null, { type: "report", failure });
    expect(v2RuntimeFailureReducer(reported, { type: "dismiss" })).toBeNull();
    expect(v2RuntimeFailureReducer(reported, { type: "begin_takeover" })).toBeNull();
  });
});

describe("takeover confirmation and automatic microphone arm", () => {
  it("uses an explicit two-step confirmation on Windows", () => {
    expect(takeoverConfirmationMode("windows")).toBe("two_step");
    expect(takeoverConfirmationMode("android")).toBe("press_and_hold");
  });

  it("arms once only after the active claim advances ownerEpoch", () => {
    const target = confirmedTakeoverTarget();
    const advanced = takeoverMedia(5);
    expect(shouldAutoArmConfirmedTakeover(target, advanced.lease, advanced.media, true, false)).toBe(true);
    expect(shouldAutoArmConfirmedTakeover(target, advanced.lease, advanced.media, true, true)).toBe(false);

    const preClaim = takeoverMedia(4);
    expect(shouldAutoArmConfirmedTakeover(target, preClaim.lease, preClaim.media, true, false)).toBe(false);

    const otherDevice = takeoverMedia(5);
    otherDevice.lease.session.deviceId = "device_other";
    otherDevice.media.session = otherDevice.lease.session;
    expect(shouldAutoArmConfirmedTakeover(target, otherDevice.lease, otherDevice.media, true, false)).toBe(false);
  });

  it("waits when a remote track arrives before the peer is connected", () => {
    const target = confirmedTakeoverTarget();
    const advanced = takeoverMedia(5);
    advanced.media.phase = "connecting";

    expect(shouldAutoArmConfirmedTakeover(target, advanced.lease, advanced.media, true, false)).toBe(false);
    expect(isRetryableMediaArmFailure(new Error("native WebRTC is still connecting; retry microphone arm"))).toBe(true);
    expect(isRetryableMediaArmFailure(new Error("native microphone authority is still connecting; retry microphone arm"))).toBe(true);
    expect(isRetryableMediaArmFailure(new Error("microphone authority channel is not open"))).toBe(true);
    expect(isRetryableMediaArmFailure(new Error("microphone permission was denied"))).toBe(false);
  });

  it("retries once when the authority channel opens after peer connected", () => {
    const target = confirmedTakeoverTarget();
    const advanced = takeoverMedia(5);
    // PeerConnection Connected may precede the ordered DTLS channel Open.
    // The first native arm returns retryable; authority-ready emits the later
    // exact-session revision that releases this attempt fence.
    advanced.media.phase = "connected";

    let attempt = v2TakeoverAttemptReducer(INITIAL_V2_TAKEOVER_ATTEMPT, { type: "begin" });
    const generation = attempt.generation;
    attempt = v2TakeoverAttemptReducer(attempt, { type: "target_confirmed", generation, target });
    let alreadyAttempted = false;
    let armAttempts = 0;
    let nativeMediaRevision = 4;
    const runAutoArmEffect = () => {
      if (!shouldAutoArmConfirmedTakeover(
        attempt.target,
        advanced.lease,
        advanced.media,
        true,
        alreadyAttempted,
        attempt.autoArmRequiresConnected,
        nativeMediaRevision,
        attempt.autoArmRetryAfterMediaRevision,
      )) return;
      alreadyAttempted = true;
      armAttempts += 1;
      attempt = v2TakeoverAttemptReducer(attempt, { type: "auto_arm_started" });
    };

    runAutoArmEffect();
    expect(armAttempts).toBe(1);
    expect(attempt.target).toBeNull();
    expect(isRetryableMediaArmFailure(
      new Error("native microphone authority is still connecting; retry microphone arm"),
    )).toBe(true);
    alreadyAttempted = false;
    attempt = v2TakeoverAttemptReducer(attempt, {
      type: "auto_arm_retryable",
      generation,
      target,
      armStartMediaRevision: nativeMediaRevision,
    });

    runAutoArmEffect();
    expect(armAttempts).toBe(1);

    runAutoArmEffect();
    expect(armAttempts).toBe(1);

    nativeMediaRevision += 1;
    runAutoArmEffect();
    expect(armAttempts).toBe(2);
    expect(attempt.target).toBeNull();

    // A second retryable rejection cannot restore the consumed target, even
    // if native media keeps publishing connected revisions.
    alreadyAttempted = false;
    attempt = v2TakeoverAttemptReducer(attempt, {
      type: "auto_arm_retryable",
      generation,
      target,
      armStartMediaRevision: nativeMediaRevision,
    });
    nativeMediaRevision += 1;
    runAutoArmEffect();
    expect(armAttempts).toBe(2);
    expect(attempt.target).toBeNull();
    expect(attempt.autoArmRetryCount).toBe(1);
  });

  it("does not lose a connected recovery event that beats the rejection callback", () => {
    const target = confirmedTakeoverTarget();
    const advanced = takeoverMedia(5);
    advanced.media.phase = "connected";
    let attempt = v2TakeoverAttemptReducer(INITIAL_V2_TAKEOVER_ATTEMPT, { type: "begin" });
    const generation = attempt.generation;
    attempt = v2TakeoverAttemptReducer(attempt, { type: "target_confirmed", generation, target });
    attempt = v2TakeoverAttemptReducer(attempt, { type: "auto_arm_started" });

    // Revision 4 qualified the first attempt. Revision 5 published connected
    // before its promise rejection reached JavaScript, so it must remain valid
    // recovery evidence when the retry target is restored.
    attempt = v2TakeoverAttemptReducer(attempt, {
      type: "auto_arm_retryable",
      generation,
      target,
      armStartMediaRevision: 4,
    });
    expect(shouldAutoArmConfirmedTakeover(
      attempt.target,
      advanced.lease,
      advanced.media,
      true,
      false,
      attempt.autoArmRequiresConnected,
      5,
      attempt.autoArmRetryAfterMediaRevision,
    )).toBe(true);
  });

  it("fences a pending effect against a newer authoritative sequence", () => {
    const rendered = activeTakeoverSnapshot();
    expect(currentV2AutoArmSnapshot(rendered, rendered)).toBe(rendered);

    const revoked = structuredClone(rendered);
    revoked.sequence += 1;
    revoked.snapshot.remoteConsent.acknowledged = false;
    expect(currentV2AutoArmSnapshot(rendered, revoked)).toBeNull();
  });

  it("requires current exact consent, grants, capability, and a safe handoff state", () => {
    const target = confirmedTakeoverTarget();
    const active = activeTakeoverSnapshot();
    const now = OFFER_NOW * 1_000;
    expect(v2SnapshotMaintainsConfirmedTakeoverAuthority(target, active, now)).toBe(true);
    expect(v2SnapshotAuthorizesConfirmedTakeoverArm(target, active, now)).toBe(true);
    for (const serviceMode of ["aokie_active", "human_pending", "human_active"] as const) {
      const safeHandoff = structuredClone(active);
      safeHandoff.snapshot.serviceMode = serviceMode;
      expect(v2SnapshotAuthorizesConfirmedTakeoverArm(target, safeHandoff, now)).toBe(true);
    }
    expect(v2SnapshotMaintainsConfirmedTakeoverAuthority(
      { ...target, requiredGrants: [...target.requiredGrants, "captions_read"] },
      active,
      now,
    )).toBe(false);
    const awaitingFirstMicrophonePcm = structuredClone(active);
    awaitingFirstMicrophonePcm.snapshot.mediaState = "connecting";
    expect(v2SnapshotAuthorizesConfirmedTakeoverArm(target, awaitingFirstMicrophonePcm, now)).toBe(true);

    const mutations: Array<(snapshot: V2CallSnapshotEvent) => void> = [
      (snapshot) => { snapshot.snapshot.remoteConsent.acknowledged = false; },
      (snapshot) => { snapshot.snapshot.remoteConsent.takeoverEnabled = false; },
      (snapshot) => { snapshot.snapshot.remoteConsent.policyVersion += 1; },
      (snapshot) => { snapshot.snapshot.remoteConsent.expiresAt = new Date(now - 1).toISOString(); },
      (snapshot) => { snapshot.grants = snapshot.grants.filter((grant) => grant !== "rtc_signal"); },
      (snapshot) => { snapshot.grants = snapshot.grants.filter((grant) => grant !== "takeover"); },
      (snapshot) => { snapshot.grants = snapshot.grants.filter((grant) => grant !== "resume_aokie"); },
      (snapshot) => { snapshot.snapshot.remoteCapabilities.takeover = false; },
      (snapshot) => { snapshot.snapshot.serviceMode = "recovering"; },
    ];
    for (const mutate of mutations) {
      const revoked = structuredClone(active);
      mutate(revoked);
      expect(v2SnapshotMaintainsConfirmedTakeoverAuthority(target, revoked, now)).toBe(false);
      expect(v2SnapshotAuthorizesConfirmedTakeoverArm(target, revoked, now)).toBe(false);
    }
  });

  it("refuses a rendered confirmation reset before its effect can arm", () => {
    const target = confirmedTakeoverTarget();
    let rendered = v2TakeoverAttemptReducer(INITIAL_V2_TAKEOVER_ATTEMPT, { type: "begin" });
    rendered = v2TakeoverAttemptReducer(rendered, {
      type: "target_confirmed",
      generation: rendered.generation,
      target,
    });
    const current = v2TakeoverAttemptReducer(rendered, { type: "failed_or_idle" });

    expect(currentV2AutoArmAttempt(rendered, current)).toBeNull();
  });

  it("keeps explicit consent across the expected prepared-to-active peer replacement only", () => {
    const target = confirmedTakeoverTarget();
    const prepared = takeoverMedia(4);
    prepared.lease.phase = "prepared";
    prepared.lease.provisional = true;
    prepared.lease.session.mode = "prepared_talk";
    prepared.media.session = prepared.lease.session;
    prepared.media.phase = "replaced";

    expect(isExpectedPreparedTakeoverReplacement(target, prepared.lease, prepared.media)).toBe(true);
    expect(isExpectedPreparedTakeoverReplacement(
      target,
      prepared.lease,
      { ...prepared.media, phase: "failed" },
    )).toBe(false);
    expect(isExpectedPreparedTakeoverReplacement(
      { ...target, callId: "call_other" },
      prepared.lease,
      prepared.media,
    )).toBe(false);
  });
});

describe("lease exit presentation", () => {
  it("uses an action-specific label without changing the lease mode", () => {
    expect(v2LeaseExitLabel("monitor")).toBe("Stop listening");
    expect(v2LeaseExitLabel("consult")).toBe("Finish private consult");
    expect(v2LeaseExitLabel("takeover")).toBe("Return to Aokie");
  });

  it("keeps an exact live lease revocable after takeover grants narrow", () => {
    const { lease } = takeoverMedia();
    expect(v2LeaseRevokeAllowed(true, lease)).toBe(true);
    expect(v2LeaseRevokeAllowed(false, lease)).toBe(false);
    expect(v2LeaseRevokeAllowed(true, null)).toBe(false);
  });
});

describe("current live access policy presentation", () => {
  it("explains when the current remote disclosure is not acknowledged", () => {
    const snapshot = takeoverSnapshot();
    snapshot.snapshot.remoteConsent.acknowledged = false;

    expect(v2CurrentAccessPolicyPresentation(snapshot, false)).toEqual({
      title: "Current remote access is not acknowledged",
      detail: expect.stringContaining("currently published disclosure and acknowledgement"),
    });
  });

  it("explains current live-media grant limits without making hardware claims", () => {
    const snapshot = takeoverSnapshot();
    snapshot.grants = snapshot.grants.filter((grant) => grant !== "rtc_signal");
    const presentation = v2CurrentAccessPolicyPresentation(snapshot, true);

    expect(presentation?.title).toBe("Current live-media authority is limited");
    expect(presentation?.detail).toContain("current FormLogic grants");
    expect(presentation?.detail).not.toMatch(/hardware|revoked|permanent/i);
  });

  it("explains when mode policy and device grants have no authorized intersection", () => {
    const snapshot = takeoverSnapshot();
    snapshot.grants = ["state_read", "rtc_signal"];
    expect(v2CurrentAccessPolicyPresentation(snapshot, true)?.title)
      .toBe("Current device policy permits no live actions");

    snapshot.snapshot.remoteConsent.monitorEnabled = false;
    snapshot.snapshot.remoteConsent.consultEnabled = false;
    snapshot.snapshot.remoteConsent.takeoverEnabled = false;
    expect(v2CurrentAccessPolicyPresentation(snapshot, true)?.title)
      .toBe("Current remote policy permits no live actions");
  });

  it("stays silent when at least one current policy path is authorized", () => {
    expect(v2CurrentAccessPolicyPresentation(takeoverSnapshot(), true)).toBeNull();
    expect(v2CurrentAccessPolicyPresentation(null, false)).toBeNull();
  });
});

describe("authoritative media transition guidance", () => {
  it("advances takeover guidance only from published hold and native media facts", () => {
    expect(v2RouteProgressStages("takeover", "human_pending", "prepared", false, false).map((stage) => stage.status))
      .toEqual(["done", "done", "active", "waiting"]);
    expect(v2RouteProgressStages("takeover", "human_active", "active", true, true).map((stage) => stage.status))
      .toEqual(["done", "done", "done", "done"]);
    expect(v2RouteProgressStages("takeover", "aokie_active", "prepared", true, true).map((stage) => stage.status))
      .toEqual(["done", "active", "waiting", "waiting"]);
  });

  it("keeps recovery on the first unsafe fact and never manufactures completion", () => {
    expect(v2RecoveryProgressStages("reconnecting", true, true).map((stage) => stage.status))
      .toEqual(["done", "active", "waiting", "waiting"]);
    expect(v2RecoveryProgressStages("connected", false, false).map((stage) => stage.status))
      .toEqual(["done", "done", "done", "active"]);
  });
});

describe("assistance call fencing", () => {
  const request: V2AssistanceRequestEvent = {
    kind: "assistance_request",
    schemaVersion: 2,
    appId: "app_a",
    eventId: "event_help_a",
    requestId: "request_help_a",
    callId: "call_a",
    callEpoch: 7,
    ownerEpoch: 4,
    switchboardRevision: 11,
    remoteRevision: 13,
    question: "Can we accept the booking?",
    transferOffered: false,
    expiresAt: OFFER_NOW + 30,
  };

  it("enables help only for the exact active authoritative call fence", () => {
    const call = takeoverSnapshot().snapshot;
    expect(assistanceMatchesV2Call(request, call)).toBe(true);
    expect(assistanceMatchesV2Call({ ...request, ownerEpoch: 5 }, call)).toBe(false);
    expect(assistanceMatchesV2Call(request, { ...call, telephonyState: "ended", serviceMode: "ended" })).toBe(false);
  });
});

describe("v2TakeoverAttemptReducer", () => {
  const target = confirmedTakeoverTarget();

  it("does not resurrect pending status when an error beats the enqueue receipt", () => {
    let state = v2TakeoverAttemptReducer(INITIAL_V2_TAKEOVER_ATTEMPT, { type: "begin" });
    const failedGeneration = state.generation;
    state = v2TakeoverAttemptReducer(state, { type: "target_confirmed", generation: failedGeneration, target });
    state = v2TakeoverAttemptReducer(state, { type: "failed_or_idle" });
    state = v2TakeoverAttemptReducer(state, { type: "request_enqueued", generation: failedGeneration });

    expect(state.requestPending).toBe(false);
    expect(state.target).toBeNull();
    expect(state.generation).toBeGreaterThan(failedGeneration);
  });

  it("preserves the accepted status and confirmation target across workspace navigation", () => {
    let state = v2TakeoverAttemptReducer(INITIAL_V2_TAKEOVER_ATTEMPT, { type: "begin" });
    const generation = state.generation;
    state = v2TakeoverAttemptReducer(state, { type: "target_confirmed", generation, target });
    state = v2TakeoverAttemptReducer(state, { type: "request_enqueued", generation });
    const navigated = v2TakeoverAttemptReducer(state, { type: "workspace_navigation" });

    expect(navigated).toBe(state);
    expect(navigated.requestPending).toBe(true);
    expect(navigated.target).toEqual(target);
  });

  it("invalidates preserved consent when a published takeover lease is reset", () => {
    let state = v2TakeoverAttemptReducer(INITIAL_V2_TAKEOVER_ATTEMPT, { type: "begin" });
    const confirmedGeneration = state.generation;
    state = v2TakeoverAttemptReducer(state, { type: "target_confirmed", generation: confirmedGeneration, target });
    state = v2TakeoverAttemptReducer(state, { type: "lease_published" });
    expect(state.target).toEqual(target);

    state = v2TakeoverAttemptReducer(state, { type: "failed_or_idle" });
    expect(state.target).toBeNull();
    expect(state.requestPending).toBe(false);
    expect(state.generation).toBeGreaterThan(confirmedGeneration);
  });

  it("does not restore a retryable auto-arm after the attempt fence advances", () => {
    let state = v2TakeoverAttemptReducer(INITIAL_V2_TAKEOVER_ATTEMPT, { type: "begin" });
    const staleGeneration = state.generation;
    state = v2TakeoverAttemptReducer(state, { type: "target_confirmed", generation: staleGeneration, target });
    state = v2TakeoverAttemptReducer(state, { type: "auto_arm_started" });
    state = v2TakeoverAttemptReducer(state, { type: "failed_or_idle" });
    state = v2TakeoverAttemptReducer(state, {
      type: "auto_arm_retryable",
      generation: staleGeneration,
      target,
      armStartMediaRevision: 3,
    });

    expect(state.target).toBeNull();
    expect(state.autoArmRequiresConnected).toBe(false);
  });

  it("spends a restored retry target synchronously when Return to Aokie clears it", () => {
    let state = v2TakeoverAttemptReducer(INITIAL_V2_TAKEOVER_ATTEMPT, { type: "begin" });
    const generation = state.generation;
    state = v2TakeoverAttemptReducer(state, { type: "target_confirmed", generation, target });
    state = v2TakeoverAttemptReducer(state, { type: "auto_arm_started" });
    state = v2TakeoverAttemptReducer(state, {
      type: "auto_arm_retryable",
      generation,
      target,
      armStartMediaRevision: 3,
    });
    expect(state.target).toEqual(target);

    state = v2TakeoverAttemptReducer(state, { type: "failed_or_idle" });
    expect(state.target).toBeNull();
    expect(state.autoArmRetryCount).toBe(0);
    expect(state.generation).toBeGreaterThan(generation);
  });
});
