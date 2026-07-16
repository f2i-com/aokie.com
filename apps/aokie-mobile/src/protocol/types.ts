export const COMPANION_SCHEMA_VERSION = 1 as const;
export const MAX_JSON_SAFE_INTEGER = 9_007_199_254_740_991 as const;

export type TelephonyState = "ringing" | "active" | "held" | "ending" | "ended";

export type ServiceMode =
  | "aokie_active"
  | "soft_hold"
  | "consult_pending"
  | "consult_active"
  | "human_pending"
  | "human_active"
  | "returning_to_aokie"
  | "recovering"
  | "unreachable"
  | "ended";

export type MediaState = "none" | "ready" | "receiving" | "connecting" | "active" | "failed";

export type TalkOwner =
  | { kind: "aokie" }
  | { kind: "hold" }
  | { kind: "user"; userId: string; deviceId: string; leaseId: string; fence: number }
  | { kind: "none" };

export interface RemoteCapabilities {
  liveCaptions: boolean;
  monitorAudio: boolean;
  softwareHold: boolean;
  voiceConsult: boolean;
  takeover: boolean;
  endCaller: boolean;
}

export interface TakeoverOffer {
  offerId: string;
  expiresAt: string;
}

export interface AssistanceRequest {
  requestId: string;
  expiresAt: string;
}

export interface EndConfirmation {
  confirmationId: string;
  expiresAt: string;
}

export interface CallSnapshot {
  schemaVersion: typeof COMPANION_SCHEMA_VERSION;
  appId: string;
  streamNonce: string;
  callId: string;
  sequence: number;
  callEpoch: number;
  ownerEpoch: number;
  switchboardRevision: number;
  remoteRevision: number;
  telephonyState: TelephonyState;
  serviceMode: ServiceMode;
  talkOwner: TalkOwner;
  mediaState: MediaState;
  gatewayReachable: boolean;
  capabilities: RemoteCapabilities;
  disclosure: {
    required: boolean;
    verified: boolean;
    policyVersion: string;
  };
  takeoverOffer?: TakeoverOffer | null;
  assistanceRequest?: AssistanceRequest | null;
  endConfirmation?: EndConfirmation | null;
  secondaryCallPolicy: "miss_and_callback";
  caller?: { label?: string | null; maskedNumber?: string | null } | null;
  participants: Array<{
    userId: string;
    deviceId: string;
    displayName: string;
    role: string;
    mode: string;
  }>;
  captions: Array<{
    captionId: string;
    speaker: string;
    text: string;
    occurredAt: string;
    finalText: boolean;
  }>;
  occurredAt: string;
}

export interface CommandPayloads {
  monitor_start: Record<string, never>;
  monitor_stop: Record<string, never>;
  assistance_respond: { requestId: string; answerId: string; answer: string };
  consult_claim: { offerId: string };
  consult_end: { leaseId: string };
  takeover_claim: { offerId: string };
  resume_aokie: { leaseId: string; reason: string };
  end_caller: { confirmationId: string };
}

export type CommandType = keyof CommandPayloads;

interface CommandEnvelopeBase {
  schemaVersion: typeof COMPANION_SCHEMA_VERSION;
  commandId: string;
  idempotencyKey: string;
  callId: string;
  expectedSwitchboardRevision: number;
  expectedRemoteRevision: number;
  expectedCallEpoch: number;
  expectedOwnerEpoch: number;
}

export type CommandEnvelope = {
  [Type in CommandType]: CommandEnvelopeBase & {
    type: Type;
    payload: CommandPayloads[Type];
  };
}[CommandType];

export type ProtocolErrorCode =
  | "invalid_message"
  | "unauthorized"
  | "forbidden"
  | "stale_call"
  | "stale_call_epoch"
  | "stale_owner_epoch"
  | "stale_remote_revision"
  | "stale_switchboard_revision"
  | "offer_expired"
  | "claim_lost"
  | "lease_expired"
  | "media_unavailable"
  | "endpoint_unreachable"
  | "command_failed";

export interface CommandAck {
  commandId: string;
  accepted: boolean;
  error?: { code: ProtocolErrorCode; message: string };
}

export const STALE_COMMAND_ERRORS = new Set<ProtocolErrorCode>([
  "stale_call",
  "stale_call_epoch",
  "stale_owner_epoch",
  "stale_remote_revision",
  "stale_switchboard_revision",
]);
