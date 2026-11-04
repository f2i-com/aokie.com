import { describe, expect, it } from "vitest";
import type { DesktopPeerTrustChallenge, RealtimeConfig } from "../../bridge/CompanionBridge";
import { settlePeerTrust, type PeerTrustDecisionSteps } from "./peerTrustDecision";

const challenge = { challengeId: "peer_trust_1" } as unknown as DesktopPeerTrustChallenge;
const config = { deviceId: "device_1" } as unknown as RealtimeConfig;

function recorder(failConfirm = false) {
  const calls: string[] = [];
  const steps: PeerTrustDecisionSteps = {
    async confirm(_challenge, approved) {
      calls.push(`confirm:${approved}`);
      if (failConfirm) throw new Error("native refused");
    },
    async disconnect() {
      calls.push("disconnect");
    },
    clear() {
      calls.push("clear");
    },
    async reconnect() {
      calls.push("reconnect");
    },
  };
  return { calls, steps };
}

describe("settlePeerTrust", () => {
  it("stops the connection when the owner rejects the key, so its retries cannot ask again", async () => {
    const { calls, steps } = recorder();
    await settlePeerTrust(steps, challenge, false, config);
    expect(calls).toEqual(["confirm:false", "disconnect"]);
  });

  it("stops the connection on a rejection even when telling the native side fails", async () => {
    const { calls, steps } = recorder(true);
    await expect(settlePeerTrust(steps, challenge, false, config)).rejects.toThrow("native refused");
    expect(calls).toEqual(["confirm:false", "disconnect"]);
  });

  it("closes the comparison before connecting again with an approved key", async () => {
    const { calls, steps } = recorder();
    await settlePeerTrust(steps, challenge, true, config);
    expect(calls).toEqual(["confirm:true", "clear", "reconnect"]);
  });

  it("does not connect when there is nothing to retry", async () => {
    const { calls, steps } = recorder();
    await settlePeerTrust(steps, challenge, true, null);
    expect(calls).toEqual(["confirm:true", "clear"]);
  });
});
