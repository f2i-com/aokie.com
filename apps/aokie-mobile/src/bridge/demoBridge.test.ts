import { describe, expect, it } from "vitest";
import { createCommand } from "../protocol/codec";
import type { BridgeEvent } from "./CompanionBridge";
import { DemoCompanionBridge } from "./demoBridge";
import { createCompanionBridge } from "./index";

describe("DemoCompanionBridge", () => {
  it("never substitutes demo data for an ordinary browser connection", () => {
    expect(createCompanionBridge().kind).toBe("unavailable");
  });

  it("labels itself as demo and requires an explicit untrusted discovery state", async () => {
    const bridge = new DemoCompanionBridge();
    expect((await bridge.getRuntimeCapabilities()).demo).toBe(true);
    expect((await bridge.discover("https://ignored.invalid")).signatureVerified).toBe(false);
  });

  it("emits a lease-bound human snapshot only after the takeover command ack", async () => {
    const bridge = new DemoCompanionBridge();
    const events: BridgeEvent[] = [];
    bridge.subscribe((event) => events.push(event));
    await bridge.connect({
      gatewayUrl: "wss://demo.invalid",
      appId: "app_demo",
      deviceId: "device_demo",
      accessToken: "demo-token-is-never-production",
    });
    const initial = events.find((event) => event.type === "snapshot");
    if (!initial || initial.type !== "snapshot") throw new Error("missing demo snapshot");
    await bridge.send(createCommand(initial.value, "device_demo", "takeover_claim", {
      offerId: "offer_demo",
    }));
    const snapshots = events.filter((event) => event.type === "snapshot");
    const latest = snapshots[snapshots.length - 1];
    expect(latest?.type).toBe("snapshot");
    if (latest?.type === "snapshot") {
      expect(latest.value.serviceMode).toBe("human_active");
      expect(latest.value.talkOwner.kind).toBe("user");
    }
  });
});
