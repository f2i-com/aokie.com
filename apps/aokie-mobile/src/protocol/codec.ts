import {
  COMPANION_SCHEMA_VERSION,
  MAX_JSON_SAFE_INTEGER,
  type CallSnapshot,
  type CommandAck,
  type CommandEnvelope,
  type CommandPayloads,
  type CommandType,
  type MediaState,
  type ProtocolErrorCode,
  type ServiceMode,
  type TelephonyState,
  type TalkOwner,
} from "./types";

const SAFE_ID = /^[A-Za-z0-9._:-]{1,200}$/;
const IDEMPOTENCY_DEVICE_LIMIT = 152;
export const MAX_ASSISTANCE_ANSWER_CHARACTERS = 2_000;
const SERVICE_MODES = new Set<ServiceMode>([
  "aokie_active",
  "soft_hold",
  "consult_pending",
  "consult_active",
  "human_pending",
  "human_active",
  "returning_to_aokie",
  "recovering",
  "unreachable",
  "ended",
]);
const TELEPHONY_STATES = new Set<TelephonyState>(["ringing", "active", "held", "ending", "ended"]);
const MEDIA_STATES = new Set<MediaState>(["none", "ready", "receiving", "connecting", "active", "failed"]);
const ERROR_CODES = new Set<ProtocolErrorCode>([
  "invalid_message",
  "unauthorized",
  "forbidden",
  "stale_call",
  "stale_call_epoch",
  "stale_owner_epoch",
  "stale_remote_revision",
  "stale_switchboard_revision",
  "offer_expired",
  "claim_lost",
  "lease_expired",
  "media_unavailable",
  "endpoint_unreachable",
  "command_failed",
]);

export class CompanionProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "CompanionProtocolError";
  }
}

function object(value: unknown, name: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new CompanionProtocolError(`${name} must be an object`);
  }
  return value as Record<string, unknown>;
}

function onlyKeys(value: Record<string, unknown>, name: string, keys: readonly string[]): void {
  const allowed = new Set(keys);
  const unknown = Object.keys(value).find((key) => !allowed.has(key));
  if (unknown) throw new CompanionProtocolError(`${name}.${unknown} is not supported`);
}

function safeId(value: unknown, name: string): string {
  if (typeof value !== "string" || !SAFE_ID.test(value)) {
    throw new CompanionProtocolError(`${name} is not a safe identifier`);
  }
  return value;
}

function boundedText(value: unknown, name: string, max: number, allowEmpty = false): string {
  const hasControl = typeof value === "string" && [...value].some((character) => {
    const code = character.codePointAt(0) ?? 0;
    return (code < 32 || code === 127) && character !== "\n" && character !== "\r" && character !== "\t";
  });
  if (
    typeof value !== "string" ||
    [...value].length > max ||
    hasControl ||
    (!allowEmpty && value.trim().length === 0)
  ) {
    throw new CompanionProtocolError(`${name} must be ${allowEmpty ? `at most ${max}` : `1-${max}`} characters`);
  }
  return value;
}

function nullableText(value: unknown, name: string, max: number): string | null {
  return value === null ? null : boundedText(value, name, max);
}

function boundedIdentifier(value: unknown, name: string, max: number): string {
  if (typeof value !== "string" || value.length > max || !SAFE_ID.test(value)) {
    throw new CompanionProtocolError(`${name} is not a bounded safe identifier`);
  }
  return value;
}

function booleanValue(value: unknown, name: string): boolean {
  if (typeof value !== "boolean") throw new CompanionProtocolError(`${name} must be boolean`);
  return value;
}

function nonNegativeInteger(value: unknown, name: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Number(value) > MAX_JSON_SAFE_INTEGER) {
    throw new CompanionProtocolError(`${name} must be a JSON-safe non-negative integer`);
  }
  return Number(value);
}

function dateTime(value: unknown, name: string): string {
  const text = boundedText(value, name, 64);
  const shape = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/;
  if (!shape.test(text) || !Number.isFinite(Date.parse(text))) {
    throw new CompanionProtocolError(`${name} must be an ISO date-time`);
  }
  return text;
}

function enumValue<Type extends string>(value: unknown, name: string, values: Set<Type>): Type {
  if (typeof value !== "string" || !values.has(value as Type)) {
    throw new CompanionProtocolError(`${name} is not supported`);
  }
  return value as Type;
}

function parseTalkOwner(value: unknown): TalkOwner {
  const owner = object(value, "talkOwner");
  if (owner.kind === "aokie" || owner.kind === "hold" || owner.kind === "none") {
    onlyKeys(owner, "talkOwner", ["kind"]);
    return { kind: owner.kind };
  }
  if (owner.kind !== "user") throw new CompanionProtocolError("talkOwner.kind is not supported");
  onlyKeys(owner, "talkOwner", ["kind", "userId", "deviceId", "leaseId", "fence"]);
  const fence = nonNegativeInteger(owner.fence, "talkOwner.fence");
  if (fence < 1) throw new CompanionProtocolError("talkOwner.fence must be at least one");
  return {
    kind: "user",
    userId: safeId(owner.userId, "talkOwner.userId"),
    deviceId: safeId(owner.deviceId, "talkOwner.deviceId"),
    leaseId: safeId(owner.leaseId, "talkOwner.leaseId"),
    fence,
  };
}

function validateOwnerMatrix(snapshot: {
  serviceMode: ServiceMode;
  telephonyState: TelephonyState;
  mediaState: MediaState;
  talkOwner: TalkOwner;
}): void {
  const holdModes = new Set<ServiceMode>([
    "soft_hold",
    "consult_pending",
    "consult_active",
    "human_pending",
    "returning_to_aokie",
    "recovering",
    "unreachable",
  ]);
  if (snapshot.serviceMode === "aokie_active" && snapshot.talkOwner.kind !== "aokie") {
    throw new CompanionProtocolError("aokie_active requires Aokie to own the talk path");
  }
  if (holdModes.has(snapshot.serviceMode) && snapshot.talkOwner.kind !== "hold") {
    throw new CompanionProtocolError(`${snapshot.serviceMode} requires the caller to remain on hold`);
  }
  if (snapshot.serviceMode === "human_active" && snapshot.talkOwner.kind !== "user") {
    throw new CompanionProtocolError("human_active requires a lease-bound user talk owner");
  }
  if (snapshot.serviceMode === "human_active" && snapshot.mediaState !== "active") {
    throw new CompanionProtocolError("human_active requires confirmed active media");
  }
  if (snapshot.serviceMode === "ended") {
    if (snapshot.talkOwner.kind !== "none" || snapshot.telephonyState !== "ended" || snapshot.mediaState !== "none") {
      throw new CompanionProtocolError("ended requires ended telephony, no talk owner, and no media");
    }
  } else if (snapshot.telephonyState === "ended" || snapshot.talkOwner.kind === "none") {
    throw new CompanionProtocolError("a live service mode cannot contain ended telephony or no owner");
  }
}

function parseOpportunity(
  value: unknown,
  name: string,
  idKey: "offerId" | "requestId" | "confirmationId",
): { [key: string]: string } | null | undefined {
  if (value === undefined || value === null) return value;
  const input = object(value, name);
  onlyKeys(input, name, [idKey, "expiresAt"]);
  return { [idKey]: safeId(input[idKey], `${name}.${idKey}`), expiresAt: dateTime(input.expiresAt, `${name}.expiresAt`) };
}

/** Parse and validate an authoritative snapshot before it can influence UI safety state. */
export function parseCallSnapshot(value: unknown): CallSnapshot {
  const input = object(value, "snapshot");
  onlyKeys(input, "snapshot", [
    "schemaVersion", "appId", "streamNonce", "callId", "sequence", "callEpoch", "ownerEpoch",
    "switchboardRevision", "remoteRevision", "telephonyState", "serviceMode", "talkOwner",
    "mediaState", "gatewayReachable", "capabilities", "disclosure", "takeoverOffer",
    "assistanceRequest", "endConfirmation", "secondaryCallPolicy", "caller", "participants",
    "captions", "occurredAt",
  ]);
  if (input.schemaVersion !== COMPANION_SCHEMA_VERSION) {
    throw new CompanionProtocolError(`unsupported schemaVersion ${String(input.schemaVersion)}`);
  }
  safeId(input.appId, "appId");
  safeId(input.streamNonce, "streamNonce");
  safeId(input.callId, "callId");
  const sequence = nonNegativeInteger(input.sequence, "sequence");
  if (sequence === 0) throw new CompanionProtocolError("sequence must be greater than zero");
  nonNegativeInteger(input.callEpoch, "callEpoch");
  nonNegativeInteger(input.ownerEpoch, "ownerEpoch");
  nonNegativeInteger(input.switchboardRevision, "switchboardRevision");
  nonNegativeInteger(input.remoteRevision, "remoteRevision");
  const telephonyState = enumValue(input.telephonyState, "telephonyState", TELEPHONY_STATES);
  const serviceMode = enumValue(input.serviceMode, "serviceMode", SERVICE_MODES);
  const mediaState = enumValue(input.mediaState, "mediaState", MEDIA_STATES);
  const talkOwner = parseTalkOwner(input.talkOwner);
  booleanValue(input.gatewayReachable, "gatewayReachable");

  const capabilities = object(input.capabilities, "capabilities");
  const capabilityKeys = ["liveCaptions", "monitorAudio", "softwareHold", "voiceConsult", "takeover", "endCaller"] as const;
  onlyKeys(capabilities, "capabilities", capabilityKeys);
  for (const key of capabilityKeys) booleanValue(capabilities[key], `capabilities.${key}`);

  const disclosure = object(input.disclosure, "disclosure");
  onlyKeys(disclosure, "disclosure", ["required", "verified", "policyVersion"]);
  booleanValue(disclosure.required, "disclosure.required");
  booleanValue(disclosure.verified, "disclosure.verified");
  boundedIdentifier(disclosure.policyVersion, "disclosure.policyVersion", 100);

  parseOpportunity(input.takeoverOffer, "takeoverOffer", "offerId");
  parseOpportunity(input.assistanceRequest, "assistanceRequest", "requestId");
  parseOpportunity(input.endConfirmation, "endConfirmation", "confirmationId");
  if (input.secondaryCallPolicy !== "miss_and_callback") {
    throw new CompanionProtocolError("unsupported secondaryCallPolicy");
  }

  if (input.caller !== null && input.caller !== undefined) {
    const caller = object(input.caller, "caller");
    onlyKeys(caller, "caller", ["label", "maskedNumber"]);
    if (caller.label !== undefined) nullableText(caller.label, "caller.label", 200);
    if (caller.maskedNumber !== undefined) nullableText(caller.maskedNumber, "caller.maskedNumber", 40);
  }

  if (!Array.isArray(input.participants) || input.participants.length > 6) {
    throw new CompanionProtocolError("participants must contain at most six entries");
  }
  input.participants.forEach((value, index) => {
    const participant = object(value, `participants[${index}]`);
    onlyKeys(participant, `participants[${index}]`, ["userId", "deviceId", "displayName", "role", "mode"]);
    safeId(participant.userId, `participants[${index}].userId`);
    safeId(participant.deviceId, `participants[${index}].deviceId`);
    boundedText(participant.displayName, `participants[${index}].displayName`, 120);
    boundedIdentifier(participant.role, `participants[${index}].role`, 40);
    boundedIdentifier(participant.mode, `participants[${index}].mode`, 40);
  });

  if (!Array.isArray(input.captions) || input.captions.length > 200) {
    throw new CompanionProtocolError("captions must contain at most 200 entries");
  }
  if (!booleanValue(capabilities.liveCaptions, "capabilities.liveCaptions") && input.captions.length > 0) {
    throw new CompanionProtocolError("captions require capabilities.liveCaptions");
  }
  input.captions.forEach((value, index) => {
    const caption = object(value, `captions[${index}]`);
    onlyKeys(caption, `captions[${index}]`, ["captionId", "speaker", "text", "occurredAt", "finalText"]);
    safeId(caption.captionId, `captions[${index}].captionId`);
    boundedIdentifier(caption.speaker, `captions[${index}].speaker`, 40);
    boundedText(caption.text, `captions[${index}].text`, 2_000);
    dateTime(caption.occurredAt, `captions[${index}].occurredAt`);
    booleanValue(caption.finalText, `captions[${index}].finalText`);
  });
  dateTime(input.occurredAt, "occurredAt");
  validateOwnerMatrix({ serviceMode, telephonyState, mediaState, talkOwner });
  return input as unknown as CallSnapshot;
}

function randomId(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function validatePayload<Type extends CommandType>(type: Type, value: CommandPayloads[Type]): CommandPayloads[Type] {
  const payload = object(value, "payload");
  switch (type) {
    case "monitor_start":
    case "monitor_stop":
      onlyKeys(payload, "payload", []);
      break;
    case "assistance_respond":
      onlyKeys(payload, "payload", ["requestId", "answerId", "answer"]);
      safeId(payload.requestId, "payload.requestId");
      safeId(payload.answerId, "payload.answerId");
      boundedText(payload.answer, "payload.answer", MAX_ASSISTANCE_ANSWER_CHARACTERS);
      break;
    case "consult_claim":
    case "takeover_claim":
      onlyKeys(payload, "payload", ["offerId"]);
      safeId(payload.offerId, "payload.offerId");
      break;
    case "consult_end":
      onlyKeys(payload, "payload", ["leaseId"]);
      safeId(payload.leaseId, "payload.leaseId");
      break;
    case "resume_aokie":
      onlyKeys(payload, "payload", ["leaseId", "reason"]);
      safeId(payload.leaseId, "payload.leaseId");
      boundedText(payload.reason, "payload.reason", 200);
      break;
    case "end_caller":
      onlyKeys(payload, "payload", ["confirmationId"]);
      safeId(payload.confirmationId, "payload.confirmationId");
      break;
  }
  return value;
}

export function createCommand<Type extends CommandType>(
  snapshot: CallSnapshot,
  deviceId: string,
  type: Type,
  payload: CommandPayloads[Type],
): CommandEnvelope {
  const commandId = `cmd_${randomId()}`;
  const normalizedDeviceId = safeId(deviceId, "deviceId").slice(0, IDEMPOTENCY_DEVICE_LIMIT);
  validatePayload(type, payload);
  return {
    schemaVersion: COMPANION_SCHEMA_VERSION,
    commandId,
    idempotencyKey: `mobile:${normalizedDeviceId}:${commandId}`,
    callId: snapshot.callId,
    expectedSwitchboardRevision: snapshot.switchboardRevision,
    expectedRemoteRevision: snapshot.remoteRevision,
    expectedCallEpoch: snapshot.callEpoch,
    expectedOwnerEpoch: snapshot.ownerEpoch,
    type,
    payload,
  } as CommandEnvelope;
}

export function parseCommandAck(value: unknown): CommandAck {
  const input = object(value, "commandAck");
  onlyKeys(input, "commandAck", ["commandId", "accepted", "error"]);
  const commandId = safeId(input.commandId, "commandAck.commandId");
  const accepted = booleanValue(input.accepted, "commandAck.accepted");
  if (accepted) {
    if (input.error !== undefined && input.error !== null) {
      throw new CompanionProtocolError("an accepted command cannot include an error");
    }
    return { commandId, accepted: true };
  }
  const error = object(input.error, "commandAck.error");
  onlyKeys(error, "commandAck.error", ["code", "message"]);
  if (typeof error.code !== "string" || !ERROR_CODES.has(error.code as ProtocolErrorCode)) {
    throw new CompanionProtocolError("commandAck.error.code is not supported");
  }
  return {
    commandId,
    accepted: false,
    error: {
      code: error.code as ProtocolErrorCode,
      message: boundedText(error.message, "commandAck.error.message", 500),
    },
  };
}
