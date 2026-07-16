import { describe, expect, it } from "vitest";
import { isCurrentEndCallerChallenge, v2SafetyPresentation } from "./App";
import type { V2CallSnapshotEvent, V2EndCallerEvent, V2LeaseEvent } from "./bridge";

const session = {
  appId: "app_test",
  streamNonce: "stream_test",
  rtcSessionId: "rtc_test",
  callId: "call_test",
  callEpoch: 3,
  ownerEpoch: 7,
  deviceId: "device_test",
  mode: "monitor" as const,
  leaseId: "lease_test",
  fence: 0,
  sdpRevision: 1,
  transportGeneration: 1,
  expiresAt: "2099-07-16T01:00:00Z",
};

function lease(mode: V2LeaseEvent["mode"], phase: V2LeaseEvent["phase"] = "active"): V2LeaseEvent {
  return {
    mode,
    phase,
    provisional: phase === "prepared",
    session: {
      ...session,
      mode: mode === "takeover" ? (phase === "prepared" ? "prepared_talk" : "talk")
        : mode === "consult" ? (phase === "prepared" ? "prepared_consult" : "consult")
          : "monitor",
      fence: mode === "takeover" ? 4 : 0,
    },
  };
}

describe("authoritative v2 audio safety presentation", () => {
  it("never claims live audio from disconnected state", () => {
    expect(v2SafetyPresentation("offline", "human_active", lease("takeover"), false, true, false).title)
      .toBe("CONNECTION LOST — AUDIO BLOCKED");
  });

  it("uses the immutable receive-only monitoring banner", () => {
    expect(v2SafetyPresentation("connected", "aokie_active", lease("monitor"), true, false, false))
      .toMatchObject({ title: "LISTENING — YOU CANNOT BE HEARD" });
    expect(v2SafetyPresentation("connected", "aokie_active", lease("monitor"), false, false, false))
      .toMatchObject({ title: "CONNECTING LISTEN-ONLY — YOU CANNOT BE HEARD" });
  });

  it("keeps takeover pending until exact native proof exists", () => {
    expect(v2SafetyPresentation("connected", "human_pending", lease("takeover", "prepared"), false, false, false).title)
      .toBe("CONNECTING — YOU ARE NOT LIVE");
    expect(v2SafetyPresentation("connected", "human_active", lease("takeover"), false, false, false).title)
      .toBe("CONNECTING — YOU ARE NOT LIVE");
    expect(v2SafetyPresentation("connected", "human_active", lease("takeover"), false, true, false).title)
      .toBe("YOU ARE LIVE — CALLER CAN HEAR YOU");
  });

  it("distinguishes isolated private consult from caller takeover", () => {
    expect(v2SafetyPresentation("connected", "consult_active", lease("consult"), false, false, false).title)
      .toBe("CALLER ON HOLD — MICROPHONE BLOCKED");
    expect(v2SafetyPresentation("connected", "consult_active", lease("consult"), false, false, true).title)
      .toBe("PRIVATE WITH AOKIE — CALLER CANNOT HEAR YOU");
  });

  it("fails closed when service state says human but no lease is present", () => {
    expect(v2SafetyPresentation("connected", "human_active", null, false, false, false).title)
      .toBe("REMOTE AUDIO LOCKED — WAITING FOR CURRENT LEASE");
  });
});

const endSnapshot: V2CallSnapshotEvent = {
  kind: "snapshot",
  schemaVersion: 2,
  appId: "app_test",
  sequence: 9,
  grants: ["state_read", "takeover", "end_caller"],
  snapshot: {
    callId: "call_test",
    callEpoch: 3,
    ownerEpoch: 7,
    switchboardRevision: 11,
    remoteRevision: 13,
    telephonyState: "active",
    serviceMode: "human_active",
    mediaState: "active",
    remoteCapabilities: {
      softwareHold: true,
      carrierHoldEvidence: "proven",
      secondaryCallObservation: "observed",
      voiceConsult: true,
      takeover: true,
    },
    secondaryCallPolicy: "miss_and_callback",
    remoteConsent: {
      policyId: "policy_test",
      policyVersion: 2,
      enabled: true,
      acknowledged: true,
      captionsEnabled: true,
      assistanceEnabled: true,
      monitorEnabled: true,
      consultEnabled: true,
      takeoverEnabled: true,
    },
    participants: [],
    pendingMobileOffers: [],
    occurredAt: "2026-07-16T00:00:00Z",
  },
};

const endChallenge: V2EndCallerEvent = {
  kind: "end_caller_challenge",
  schemaVersion: 2,
  appId: "app_test",
  requestId: "request_test",
  confirmationId: "confirmation_test",
  deviceId: "device_test",
  callId: "call_test",
  callEpoch: 3,
  ownerEpoch: 7,
  switchboardRevision: 11,
  remoteRevision: 13,
  leaseId: "lease_test",
  fence: 4,
  expiresAt: 2_000_000_012,
};

describe("v2 end-caller confirmation fencing", () => {
  it("accepts only an unexpired challenge bound to the exact active takeover", () => {
    expect(isCurrentEndCallerChallenge(endChallenge, endSnapshot, lease("takeover"), 2_000_000_000)).toBe(true);
    expect(isCurrentEndCallerChallenge(endChallenge, endSnapshot, lease("takeover"), 2_000_000_012)).toBe(false);
  });

  it("rejects a revision, owner, consent, or grant mismatch", () => {
    expect(isCurrentEndCallerChallenge({ ...endChallenge, remoteRevision: 12 }, endSnapshot, lease("takeover"), 2_000_000_000)).toBe(false);
    expect(isCurrentEndCallerChallenge({ ...endChallenge, ownerEpoch: 6 }, endSnapshot, lease("takeover"), 2_000_000_000)).toBe(false);
    expect(isCurrentEndCallerChallenge(endChallenge, { ...endSnapshot, grants: ["takeover"] }, lease("takeover"), 2_000_000_000)).toBe(false);
    expect(isCurrentEndCallerChallenge(endChallenge, {
      ...endSnapshot,
      snapshot: { ...endSnapshot.snapshot, remoteConsent: { ...endSnapshot.snapshot.remoteConsent, acknowledged: false } },
    }, lease("takeover"), 2_000_000_000)).toBe(false);
  });
});
