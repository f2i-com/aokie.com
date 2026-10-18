import { describe, expect, it } from "vitest";
import { demoSnapshot } from "../features/demo/snapshot";
import {
  CompanionProtocolError,
  createCommand,
  MAX_ASSISTANCE_ANSWER_CHARACTERS,
  MAX_V2_ASSISTANCE_ANSWER_BYTES,
  parseCallSnapshot,
  parseCommandAck,
  sanitizeLine,
  truncateToUtf8Bytes,
  utf8ByteLength,
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

  it("counts protocol-v2 answers in UTF-8 bytes and never splits a character", () => {
    expect(utf8ByteLength("abc")).toBe(3);
    expect(utf8ByteLength("日本")).toBe(6);
    expect(utf8ByteLength("😀")).toBe(4);
    expect(truncateToUtf8Bytes("日本", 4)).toBe("日");
    expect(truncateToUtf8Bytes("a😀", 4)).toBe("a");
    expect(truncateToUtf8Bytes("a😀", 5)).toBe("a😀");
    // 2,000 CJK characters fit the old UTF-16 count but are 6,000 bytes.
    const cjk = "語".repeat(2_000);
    const cut = truncateToUtf8Bytes(cjk, MAX_V2_ASSISTANCE_ANSWER_BYTES);
    expect(utf8ByteLength(cut)).toBeLessThanOrEqual(MAX_V2_ASSISTANCE_ANSWER_BYTES);
    expect([...cut].length).toBe(666);
  });

  it("folds an answer to one line the way aokie-protocol does", () => {
    expect(sanitizeLine("one\ntwo", 100)).toBe("one two");
    expect(sanitizeLine("one\r\n\ttwo", 100)).toBe("one two");
    expect(sanitizeLine("a\u2028b\u2029c", 100)).toBe("a b c");
    expect(sanitizeLine("x\u0000y\u007fz\u0085w", 100)).toBe("x y z w");
    expect(sanitizeLine("\n\n lead and trail \n", 100)).toBe("lead and trail");
    expect(sanitizeLine("\n\t\r", 100)).toBe("");
    expect(sanitizeLine("“Café” — ok", 100)).toBe("“Café” — ok");
    const multi = `${"“quoted”".repeat(200)}\n${"é".repeat(800)}`;
    expect(utf8ByteLength(sanitizeLine(multi, 2_000))).toBeLessThanOrEqual(2_000);
  });
});
