import { describe, expect, it } from "vitest";
import { demoSnapshot } from "../features/demo/snapshot";
import {
  CompanionProtocolError,
  createCommand,
  MAX_ASSISTANCE_ANSWER_CHARACTERS,
  parseCallSnapshot,
  parseCommandAck,
} from "./codec";

describe("Companion protocol codec", () => {
  it("accepts the canonical safe call snapshot", () => {
    expect(parseCallSnapshot(demoSnapshot)).toEqual(demoSnapshot);
  });

  it("rejects captions when the endpoint has not granted live captions", () => {
    expect(() => parseCallSnapshot({
      ...demoSnapshot,
      capabilities: { ...demoSnapshot.capabilities, liveCaptions: false },
    })).toThrow("captions require capabilities.liveCaptions");
    expect(parseCallSnapshot({
      ...demoSnapshot,
      capabilities: { ...demoSnapshot.capabilities, liveCaptions: false },
      captions: [],
    }).captions).toEqual([]);
  });

  it("rejects a human-live claim without a lease-bound user owner", () => {
    expect(() => parseCallSnapshot({
      ...demoSnapshot,
      serviceMode: "human_active",
      talkOwner: { kind: "aokie" },
    })).toThrow(CompanionProtocolError);
  });

  it("rejects empty talk leases, unsafe hold transitions, and ended contradictions", () => {
    expect(() => parseCallSnapshot({
      ...demoSnapshot,
      serviceMode: "human_active",
      mediaState: "active",
      talkOwner: { kind: "user", userId: "", deviceId: "device", leaseId: "lease", fence: 1 },
    })).toThrow(CompanionProtocolError);
    expect(() => parseCallSnapshot({ ...demoSnapshot, serviceMode: "recovering" })).toThrow(CompanionProtocolError);
    expect(() => parseCallSnapshot({ ...demoSnapshot, telephonyState: "ended" })).toThrow(CompanionProtocolError);
  });

  it("requires a safe stream nonce and real RFC3339 timestamps", () => {
    expect(() => parseCallSnapshot({ ...demoSnapshot, streamNonce: "" }))
      .toThrow(CompanionProtocolError);
    expect(() => parseCallSnapshot({ ...demoSnapshot, occurredAt: "2026-02-30T25:00:00Z" }))
      .toThrow(CompanionProtocolError);
  });

  it("copies every applicable revision into a durable command", () => {
    const command = createCommand(demoSnapshot, "device_test", "takeover_claim", {
      offerId: "offer_test",
    });
    expect(command).toMatchObject({
      schemaVersion: 1,
      callId: demoSnapshot.callId,
      expectedSwitchboardRevision: demoSnapshot.switchboardRevision,
      expectedRemoteRevision: demoSnapshot.remoteRevision,
      expectedCallEpoch: demoSnapshot.callEpoch,
      expectedOwnerEpoch: demoSnapshot.ownerEpoch,
      type: "takeover_claim",
      payload: { offerId: "offer_test" },
    });
    expect(command.idempotencyKey).toBe(`mobile:device_test:${command.commandId}`);
  });

  it("keeps idempotency keys within the protocol limit for maximum-length device IDs", () => {
    const command = createCommand(demoSnapshot, "d".repeat(200), "monitor_start", {});
    expect(command.idempotencyKey.length).toBeLessThanOrEqual(200);
  });

  it("requires one-use command payload identifiers and validates acknowledgements", () => {
    expect(() => createCommand(demoSnapshot, "device_test", "takeover_claim", { offerId: "" }))
      .toThrow(CompanionProtocolError);
    expect(parseCommandAck({ commandId: "cmd_test", accepted: true })).toEqual({
      commandId: "cmd_test",
      accepted: true,
    });
    expect(() => parseCommandAck({ commandId: "cmd_test", accepted: false }))
      .toThrow(CompanionProtocolError);
  });

  it("bounds private assistance answers before command construction", () => {
    expect(() => createCommand(demoSnapshot, "device_test", "assistance_respond", {
      requestId: "request_test",
      answerId: "answer_test",
      answer: "a".repeat(MAX_ASSISTANCE_ANSWER_CHARACTERS + 1),
    })).toThrow(`payload.answer must be 1-${MAX_ASSISTANCE_ANSWER_CHARACTERS} characters`);
  });
});
