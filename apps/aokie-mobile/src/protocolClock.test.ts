import { afterEach, describe, expect, it, vi } from "vitest";
import {
  MAX_PROTOCOL_CLOCK_OFFSET_SECONDS,
  protocolNowMs,
  protocolNowSeconds,
  setProtocolClockOffsetSeconds,
} from "./protocolClock";

describe("protocol clock", () => {
  afterEach(() => {
    setProtocolClockOffsetSeconds(0);
    vi.useRealTimers();
  });

  it("moves this phone's clock onto the server's by native code's offset", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-18T08:00:00Z"));
    const local = Date.now();
    expect(protocolNowMs()).toBe(local);
    setProtocolClockOffsetSeconds(-42);
    expect(protocolNowMs()).toBe(local - 42_000);
    expect(protocolNowSeconds()).toBe(Math.floor(local / 1_000) - 42);
  });

  it("ignores an offset that is malformed or out of bounds", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-18T08:00:00Z"));
    setProtocolClockOffsetSeconds(30);
    const before = protocolNowMs() - Date.now();
    expect(before).toBe(30_000);
    for (const bad of [Number.NaN, 1.5, "30", null, undefined, MAX_PROTOCOL_CLOCK_OFFSET_SECONDS + 1]) {
      setProtocolClockOffsetSeconds(bad);
      expect(protocolNowMs() - Date.now()).toBe(before);
    }
  });

  it("gives a phone that trails the server the server's now for its offer and countdown gates", () => {
    // An offer the server issued now, seen by a phone 20 s behind: by the
    // phone's own clock it was issued in the future, and Take over was hidden.
    const serverNow = 1_800_000_000;
    const phoneNow = serverNow - 20;
    vi.useFakeTimers();
    vi.setSystemTime(phoneNow * 1_000);
    expect(protocolNowSeconds()).toBe(phoneNow);
    setProtocolClockOffsetSeconds(20);
    expect(protocolNowSeconds()).toBe(serverNow);
  });
});
