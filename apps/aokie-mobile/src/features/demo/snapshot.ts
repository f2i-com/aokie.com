import type { CallSnapshot } from "../../protocol/types";

export const demoSnapshot: CallSnapshot = {
  schemaVersion: 1,
  appId: "app_coastal_auto",
  streamNonce: "stream_demo_01",
  callId: "call_demo_0042",
  sequence: 42,
  callEpoch: 7,
  ownerEpoch: 12,
  switchboardRevision: 8,
  remoteRevision: 19,
  telephonyState: "active",
  serviceMode: "aokie_active",
  talkOwner: { kind: "aokie" },
  mediaState: "ready",
  gatewayReachable: true,
  capabilities: {
    liveCaptions: true,
    monitorAudio: true,
    softwareHold: true,
    voiceConsult: false,
    takeover: true,
    endCaller: true,
  },
  disclosure: {
    required: true,
    verified: true,
    policyVersion: "demo-only-1",
  },
  takeoverOffer: {
    offerId: "offer_demo",
    expiresAt: "2099-01-01T00:00:00Z",
  },
  assistanceRequest: {
    requestId: "request_demo",
    expiresAt: "2099-01-01T00:00:00Z",
  },
  endConfirmation: {
    confirmationId: "confirmation_demo",
    expiresAt: "2099-01-01T00:00:00Z",
  },
  secondaryCallPolicy: "miss_and_callback",
  caller: { label: "Mia Thompson", maskedNumber: "+61 4•• ••• 042" },
  participants: [
    {
      userId: "user_priya",
      deviceId: "device_priya_phone",
      displayName: "Priya Shah",
      role: "observer",
      mode: "captions",
    },
  ],
  captions: [
    {
      captionId: "caption_demo_0041",
      speaker: "caller",
      text: "Could I leave the keys somewhere secure?",
      occurredAt: "2026-07-15T06:00:04Z",
      finalText: true,
    },
  ],
  occurredAt: "2026-07-15T06:00:05Z",
};
