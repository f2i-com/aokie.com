import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { CompanionBridge, DesktopPairingDecision, DesktopPairingReview } from "../../bridge";
import DesktopPairingDialog, { desktopPairingStep, pairingConfirmationReady } from "./DesktopPairingDialog";

const review: DesktopPairingReview = {
  reviewId: "pairing_review_1",
  profileId: "profile_1",
  appId: "app_1",
  desktopConnectionId: "desktop_1",
  deviceId: "mobile_1",
  desktopKeyThumbprint: "A".repeat(43),
  desktopFingerprint: Array(16).fill("0123").join(":"),
  mobileKeyThumbprint: "B".repeat(43),
  mobileFingerprint: Array(16).fill("ABCD").join(":"),
  issuedAt: 100,
  expiresAt: 200,
  unsignedPublicOffer: true,
};

describe("Desktop pairing confirmation gate", () => {
  it("requires both explicit fingerprint acknowledgements and a live offer", () => {
    expect(pairingConfirmationReady(review, true, true, "Reception desk", 150)).toBe(true);
    expect(pairingConfirmationReady(null, true, true, "Reception desk", 150)).toBe(false);
    expect(pairingConfirmationReady(review, false, true, "Reception desk", 150)).toBe(false);
    expect(pairingConfirmationReady(review, true, false, "Reception desk", 150)).toBe(false);
    expect(pairingConfirmationReady(review, true, true, " ", 150)).toBe(false);
    expect(pairingConfirmationReady(review, true, true, "A".repeat(121), 150)).toBe(false);
    expect(pairingConfirmationReady(review, true, true, "Reception desk", 200)).toBe(false);
  });
});

describe("Desktop pairing progress", () => {
  const decision: DesktopPairingDecision = {
    approved: true,
    responseJson: "{\"kind\":\"aokie_mobile_pairing_response\"}",
    desktopKeyThumbprint: review.desktopKeyThumbprint,
    mobileKeyThumbprint: review.mobileKeyThumbprint,
    mobileFingerprint: review.mobileFingerprint,
    deviceId: review.deviceId,
    expiresAt: review.expiresAt,
  };

  it("tracks offer, owner verification, and Desktop approval as distinct phases", () => {
    expect(desktopPairingStep(null, null)).toBe(1);
    expect(desktopPairingStep(review, null)).toBe(2);
    expect(desktopPairingStep(review, decision)).toBe(3);
  });

  it("does not advance until a signed response actually exists", () => {
    expect(desktopPairingStep(review, { ...decision, responseJson: undefined })).toBe(2);
  });
});

describe("Desktop pairing dialog structure", () => {
  it("exposes the security flow and progress to assistive technology", () => {
    const bridge = { listServerProfiles: async () => [] } as unknown as CompanionBridge;
    const markup = renderToStaticMarkup(createElement(DesktopPairingDialog, {
      bridge,
      admission: null,
      retrying: false,
      onClose: () => undefined,
      onRetry: async () => undefined,
    }));

    expect(markup).toContain('role="dialog"');
    expect(markup).toContain('aria-describedby="desktop-pairing-description"');
    expect(markup).toContain('aria-label="Desktop pairing progress"');
    expect(markup).toContain('aria-current="step"');
    expect(markup).toContain("OWNER-CONFIRMED PAIRING");
    expect(markup).toContain("This offer does not prove Desktop’s identity");
  });
});
